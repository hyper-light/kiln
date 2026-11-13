//! The microVM harness E2E's drive and its acceptance checks (kiln plan
//! 01a1cd8e862e7169a074db3d5fa33f3d, steps 34 and 35), over kiln's public surfaces: its REST API, its
//! MCP server (which the harness calls from where it runs) and its audit trail. `tests/vm_e2e/README.md`
//! says how ROOT runs them on each host OS.
//!
//! The checks are pure and tested against samples in `vm_harnesses.rs`; the drive reads the live
//! values they judge:
//! 1. the negotiated cipher suite, read from both ends of every link the plan names
//!    ([`REQUIRED_LINKS`], [`check_all_links`]);
//! 2. nothing of a harness ran outside its microVM: no harness process in the session's process
//!    trees on the host ([`harnesses_on_host`]), and the kernel the harness says it ran on is a
//!    guest's ([`check_in_guest`]);
//! 3. the project at its host path, inside and out ([`check_project_path`], [`guest_path`]), and a
//!    file the harness wrote there read back on the host ([`check_written`]);
//! 4. a round trip to kiln's MCP server from where the harness runs ([`mcp_round_trip`]);
//! 5. an agent started from inside, held in a microVM on a runner that starts them and started
//!    through kiln's placement (`agents.start`), as the audit trail records it ([`check_child`],
//!    [`check_child_start`]).
//!
//! Step 34's own tests are `vm_harnesses.rs`'s; step 35's file builds on [`in_microvm`] and, on a Unix
//! host, [`on_host`], with whether kiln gives each harness its MCP server as its own word ([`Ask::mcp`]).

#![allow(dead_code, reason = "each test uses what it needs")]

use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::common;

/// The one TLS 1.3 suite every control link negotiates (kiln plan step 3; the plan's "every link uses
/// TLS13_AES_256_GCM_SHA384 only"), as rustls names it (`kiln_ci::facts::Negotiated::suite`). QUIC's
/// Initial packets stay AES-128-GCM under keys derived from the connection id (RFC 9001 §5): they carry
/// no application data and are no link's negotiated suite.
pub const REQUIRED_SUITE: &str = "TLS13_AES_256_GCM_SHA384";

/// The links a session in a microVM rides, each of which says its suite at both ends (kiln plan
/// 01a1cd8e862e7169a074db3d5fa33f3d, its architecture and step 34's "on every link"): kiln's server to
/// the runner holding the session (`crate::net`'s QUIC), kiln to Hex (Hex's `docs/design/hex-quic.md`),
/// the controller to the microVM's agent-control endpoint (Shards D238), and the harness to kiln's
/// MCP server, a lane of its own on agent control ([`MCP_LANE`], Shards D254). A run that reads
/// fewer never passes; a hop the built system turns out not to have is a change to this list, with
/// its reason beside it.
pub const REQUIRED_LINKS: [&str; 4] = ["server-runner", "kiln-hex", "agent-control", "mcp"];

/// kiln's MCP from inside a microVM: a lane of the agent-control link's own (the person and ROOT,
/// 2026-11-13; Shards D254), so its suite is that link's and no network grant reaches kiln for it
/// ([`grants_to_kiln`]). Shards names a member's MCP lane `mcp:<member>` (`control::Granted::name`), as
/// the session's `/runs/control/lanes` lists it (kiln plan step 17).
pub const MCP_LANE: (&str, &str) = ("agent-control", "mcp:");

/// The ten harnesses of kiln plan steps 19 to 29, by kiln's ids (`harness::kinds::Kind`).
pub const KINDS: [&str; 10] = ["claude", "codex", "gemini", "opencode", "goose", "amp", "devin", "antigravity", "cursor", "pi"];

/// What the stand-in harness says as a turn's last line, once the turn ran to its end.
pub const STUB_DONE: &str = "STUB_DONE";

// ------------------------------------------------------------------------------------------------
// What a harness said
// ------------------------------------------------------------------------------------------------

/// A line as a harness may write it: without the Markdown around a word (`` `CWD /w` ``, `**…**`).
fn bare(line: &str) -> &str {
    line.trim().trim_matches(|c| matches!(c, '`' | '*' | '"' | '\''))
}

/// Every line of what the harness said, oldest first: its replies' text, split into lines (an ACP
/// harness's reply arrives in chunks that kiln joins into one event), and its tools' output (a real
/// harness asked to print something may print it through its shell).
pub fn said(events: &[Value]) -> Vec<String> {
    let mut out = Vec::new();
    for e in events {
        let text = match e.get("kind").and_then(Value::as_str) {
            Some("assistant") => e.get("text"),
            Some("tool") => e.get("output"),
            _ => None,
        };
        for line in text.and_then(Value::as_str).unwrap_or("").lines() {
            let b = bare(line);
            if !b.is_empty() {
                out.push(b.to_owned());
            }
        }
    }
    out
}

/// What follows `key` on the last line that has it.
fn last_after(lines: &[String], key: &str) -> Option<String> {
    lines.iter().rev().find_map(|l| l.find(key).map(|i| bare(l.get(i + key.len()..).unwrap_or("")).to_owned()))
}

/// Whether the harness said `marker` (the stand-in's [`STUB_DONE`], or ROOT's for a real task).
pub fn said_marker(lines: &[String], marker: &str) -> bool {
    lines.iter().any(|l| l.contains(marker))
}

// ------------------------------------------------------------------------------------------------
// Check 1: the negotiated suite, from both ends of every link
// ------------------------------------------------------------------------------------------------

/// One end of a link: where it reports its negotiated suite (a path of kiln's API, `{session}` the
/// session's id) and the JSON pointer to the suite in that answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct End {
    pub url: String,
    pub pointer: String,
    /// Where `url` answers a list (`GET /api/agents`): the element whose field is this value
    /// (`{"name": "{machine}"}`: the session's runner), which `pointer` then reads.
    pub pick: Option<(String, String)>,
}

/// A link to read, both ends: `VM_E2E_LINKS`' entries. `lanes`: where its report lists its lanes'
/// names, for the link that carries one ([`MCP_LANE`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSpec {
    pub name: String,
    pub client: End,
    pub server: End,
    pub lanes: Option<End>,
}

/// The links of `VM_E2E_LINKS`: a JSON array of `{"name", "client": {"url", "pointer", "pick"?},
/// "server": {…}, "lanes"?: {…}}`, each named as [`REQUIRED_LINKS`] names its hop; `{session}` and
/// `{machine}` in a url or a pick are the session's id and its runner's.
/// Where agent control terminates, and so where each end says its suite, is kiln plan steps 10 and
/// 11's to decide and build, so ROOT names the endpoints.
pub fn link_specs(text: &str) -> Result<Vec<LinkSpec>, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("VM_E2E_LINKS isn't JSON: {e}"))?;
    let end = |l: &Value, which: &str| -> Result<End, String> {
        let s = |k: &str| l.pointer(&format!("/{which}/{k}")).and_then(Value::as_str).map(str::to_owned).ok_or_else(|| format!("a link's {which} has no {k}: {l}"));
        let pick = match l.pointer(&format!("/{which}/pick")) {
            None => None,
            Some(p) => match p.as_object().map(|o| o.iter().collect::<Vec<_>>()).as_deref() {
                Some([(field, Value::String(value))]) => Some(((*field).clone(), value.clone())),
                _ => return Err(format!("a link's {which} pick is one field and its value: {l}")),
            },
        };
        Ok(End { url: s("url")?, pointer: s("pointer")?, pick })
    };
    v.as_array()
        .ok_or_else(|| "VM_E2E_LINKS is a JSON array of links".to_owned())?
        .iter()
        .map(|l| {
            let name = l.get("name").and_then(Value::as_str).ok_or_else(|| format!("a link has no name: {l}"))?;
            let lanes = l.get("lanes").map(|_| end(l, "lanes")).transpose()?;
            Ok(LinkSpec { name: name.to_owned(), client: end(l, "client")?, server: end(l, "server")?, lanes })
        })
        .collect()
}

/// What each end of a link said it negotiated (`None`: it said nothing), and the lanes its report
/// listed, where its spec asks for them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSuites {
    pub name: String,
    pub client: Option<String>,
    pub server: Option<String>,
    pub lanes: Option<Vec<String>>,
}

/// The suite a body says at `pointer`.
pub fn suite_at(body: &Value, pointer: &str) -> Option<String> {
    body.pointer(pointer).and_then(Value::as_str).map(str::to_owned)
}

/// What an end reads in `body`: the body itself, or where its `pick` names one of a list's elements,
/// that element (none when no element has the field at that value).
pub fn picked<'b>(body: &'b Value, pick: Option<&(String, String)>) -> Option<&'b Value> {
    match pick {
        None => Some(body),
        Some((field, value)) => body.as_array()?.iter().find(|e| e.get(field).and_then(Value::as_str) == Some(value.as_str())),
    }
}

/// The lanes' names a body lists at `pointer`.
pub fn lanes_at(body: &Value, pointer: &str) -> Option<Vec<String>> {
    body.pointer(pointer)?.as_array()?.iter().map(|l| l.as_str().map(str::to_owned)).collect()
}

/// What kiln's runner logs for each call of its MCP it takes over a session's agent-control lane
/// (`reach::mcp::serve_over`: target `kiln.mcp.lane`, which its log leaves out, and fields `session`
/// and `tool`), its words as kiln writes them.
pub const MCP_LANE_SAID: &str = "a call of kiln's MCP over its agent-control lane (D254)";

/// The calls of kiln's MCP that a runner's log says it took over `session`'s lane: its lines of
/// [`MCP_LANE_SAID`] naming that session, as kiln writes them (`session=<id>`) or as JSON
/// (`kiln --json`: the event's `fields`).
pub fn mcp_calls_over_lane(log: &str, session: &str) -> usize {
    let said = |l: &str| match serde_json::from_str::<Value>(l) {
        Ok(v) => v.pointer("/fields/message").and_then(Value::as_str) == Some(MCP_LANE_SAID) && v.pointer("/fields/session").and_then(Value::as_str) == Some(session),
        Err(_) => l.contains(MCP_LANE_SAID) && l.split_whitespace().any(|w| w.strip_prefix("session=") == Some(session)),
    };
    log.lines().filter(|l| said(l)).count()
}

/// Ok when the link that carries kiln's MCP from inside reported its lane ([`MCP_LANE`]).
pub fn check_mcp_lane(links: &[LinkSuites]) -> Result<(), String> {
    let (carrier, lane) = MCP_LANE;
    let link = links.iter().find(|l| l.name == carrier).ok_or_else(|| format!("no {carrier} link was read, which carries kiln's MCP as a {lane}<member> lane (Shards D254)"))?;
    match &link.lanes {
        Some(lanes) if lanes.iter().any(|l| l.starts_with(lane)) => Ok(()),
        Some(lanes) => Err(format!("the {carrier} link's lanes are {lanes:?}: none is a {lane}<member> lane, on which kiln's MCP rides it (Shards D254)")),
        None => Err(format!("the {carrier} link's report listed no lanes: name where in VM_E2E_LINKS (tests/vm_e2e/README.md)")),
    }
}

/// Ok when both ends of `link` negotiated [`REQUIRED_SUITE`]; else which end said what.
pub fn check_link(link: &LinkSuites) -> Result<(), String> {
    for (which, got) in [("client", &link.client), ("server", &link.server)] {
        match got.as_deref() {
            Some(REQUIRED_SUITE) => {}
            Some(other) => return Err(format!("link {}: its {which} end negotiated {other}, not {REQUIRED_SUITE}", link.name)),
            None => return Err(format!("link {}: its {which} end said no negotiated suite", link.name)),
        }
    }
    Ok(())
}

/// Every link of [`REQUIRED_LINKS`] read, and every link read [`REQUIRED_SUITE`] at both ends. A
/// required link not read is a failure, as the acceptance needs every link's suite: a run that read
/// fewer never passes.
pub fn check_all_links(links: &[LinkSuites]) -> Result<(), String> {
    let unread: Vec<&str> = REQUIRED_LINKS.into_iter().filter(|name| !links.iter().any(|l| l.name == *name)).collect();
    if !unread.is_empty() {
        return Err(format!("no negotiated suite was read for the links {unread:?}: name each one's ends in VM_E2E_LINKS (tests/vm_e2e/README.md)"));
    }
    links.iter().try_for_each(check_link)
}

/// The machine itself, as a microVM names it (Shards D236).
pub const MACHINE: &str = "host.docker.internal";

/// The port kiln listens at unless told another, which kiln's own images name (`kiln::config`).
pub fn kiln_default_port() -> Option<u16> {
    kiln::config::Server::default().http_addr.rsplit_once(':')?.1.parse().ok()
}

/// Every stage's Agentfile directives, read with Shards' own parser.
pub fn directives(agentfile: &str) -> Result<Vec<shards_agentfile::agentfile::Directive>, String> {
    let words = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    let parsed = shards_agentfile::parser::parse_as(agentfile.as_bytes(), shards_agentfile::parser::Dialect::Agentfile).map_err(|e| format!("the Agentfile doesn't parse: {}", words(&e.message)))?;
    let linter = shards_agentfile::lint::Linter::new(shards_agentfile::lint::Config::default());
    let ins = shards_agentfile::instructions::parse(&parsed, &linter).map_err(|e| format!("the Agentfile doesn't parse: {}", words(&e.message)))?;
    Ok(ins
        .stages
        .into_iter()
        .flat_map(|s| s.commands)
        .filter_map(|c| match c.kind {
            shards_agentfile::instructions::Kind::Agentfile(d) => Some(d),
            _ => None,
        })
        .collect())
}

/// What an Agentfile grants past its microVM that reaches kiln at one of `ports`: a destination at
/// the machine itself there, a remote MCP server there, or a TCP port open to any address that takes
/// one in. Every stage's grants count, as a run can drop what its image grants and never add one
/// (D236), so these bound what any run of it reaches. kiln's MCP rides the agent-control link
/// ([`MCP_LANE`]), so an image passes with none.
pub fn grants_to_kiln(agentfile: &str, ports: &[u16]) -> Result<Vec<String>, String> {
    use shards_agentfile::agentfile::{Direction, Directive, destination_of, mcp_endpoint, port_range};
    let words = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    let at_kiln = |host: &[u8], port: u16| host.eq_ignore_ascii_case(MACHINE.as_bytes()) && ports.contains(&port);
    let mut found = Vec::new();
    for directive in &directives(agentfile)? {
        match directive {
            Directive::Network(n) => {
                for (p, direction) in n.ports.iter().filter(|(_, d)| *d != Direction::Ingress) {
                    let wide = port_range(p).is_some_and(|(proto, lo, hi)| proto == 6 && ports.iter().any(|q| (lo..=hi).contains(q)));
                    let named = matches!(destination_of(p), Some(Ok((host, port))) if *direction == Direction::Egress && at_kiln(&host, port));
                    if wide || named {
                        found.push(format!("NETWORK {} {}{}", words(&n.name), words(p), if wide { " (any address)" } else { "" }));
                    }
                }
            }
            Directive::Mcp(m) if mcp_endpoint(&m.source).is_some_and(|(host, port)| at_kiln(&host, port)) => {
                found.push(format!("MCP {} {}", words(&m.name), words(&m.source)));
            }
            _ => {}
        }
    }
    Ok(found)
}

// ------------------------------------------------------------------------------------------------
// Check 2: nothing of a harness ran outside its microVM
// ------------------------------------------------------------------------------------------------

/// A process of the host, as `ps` (Unix) or CIM (Windows) lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostProc {
    pub pid: u32,
    pub ppid: u32,
    pub argv: Vec<String>,
}

/// Programs that run other programs' files: a harness started through one names its file next.
const INTERPRETERS: &[&str] = &["python", "python3", "node", "bun", "deno", "sh", "bash", "dash", "zsh"];

/// A program's name as a harness's would be: its file name, lower-cased, without an executable's
/// or a script's extension, and without an interpreter's version (`python3.13` is `python3`).
fn program_name(word: &str) -> String {
    let file = word.rsplit(['/', '\\']).next().unwrap_or(word).to_ascii_lowercase();
    let stem = [".exe", ".cmd", ".bat", ".ps1", ".js", ".mjs", ".cjs", ".py"].iter().find_map(|x| file.strip_suffix(x)).unwrap_or(&file).to_owned();
    match stem.split_once('.') {
        Some((head, tail)) if tail.chars().all(|c| c.is_ascii_digit() || c == '.') => head.to_owned(),
        _ => stem,
    }
}

/// Whether `p` is a harness: its program, or the file an interpreter runs, is one of `programs`
/// (kiln's names for them, `Kind::program`, and the stand-in's).
fn is_harness(p: &HostProc, programs: &[&str]) -> bool {
    let first = p.argv.first().map(|w| program_name(w)).unwrap_or_default();
    let run = if INTERPRETERS.contains(&first.as_str()) { p.argv.iter().skip(1).find(|w| !w.starts_with('-')).map(|w| program_name(w)).unwrap_or_default() } else { first };
    programs.contains(&run.as_str())
}

/// Whether `p` descends from one of the processes `roots`.
fn under(p: &HostProc, roots: &[u32], procs: &[HostProc]) -> bool {
    let mut at = p.ppid;
    // A chain no longer than the processes there are: a cycle (a pid reused mid-listing) ends it.
    for _ in 0..procs.len() {
        if roots.contains(&at) {
            return true;
        }
        match procs.iter().find(|q| q.pid == at) {
            Some(q) if q.ppid != at => at = q.ppid,
            _ => return false,
        }
    }
    false
}

/// The harnesses among the processes of `roots`: the runner's (every process kiln starts for its
/// agents descends from it, the runtime it carries for microVMs among them) and any other root the
/// session's processes hang from ([`host_roots`]). A harness held in a microVM is a process of its
/// guest, never of the host, so under roots whose agents are all isolated there is none; other
/// harnesses on a shared host (a person's own, another runner's) aren't the session's and aren't
/// counted.
pub fn harnesses_on_host(procs: &[HostProc], roots: &[u32], programs: &[&str]) -> Vec<HostProc> {
    procs.iter().filter(|p| under(p, roots, procs) && is_harness(p, programs)).cloned().collect()
}

/// The roots whose process trees a drive looks in: the runner's pid, and those `VM_E2E_HOST_ROOTS`
/// names (a comma-separated list of pids: a daemon the session's microVMs run under that left the
/// runner's tree, as one that detaches does). A word there that names no pid is refused, not passed
/// over: a root dropped unseen would hide its tree.
pub fn host_roots(runner: Option<u32>) -> anyhow::Result<Vec<u32>> {
    roots_of(runner, std::env::var("VM_E2E_HOST_ROOTS").ok().as_deref()).map_err(|e| anyhow::anyhow!(e))
}

/// The runner's pid and those `named` lists ([`host_roots`]).
pub fn roots_of(runner: Option<u32>, named: Option<&str>) -> Result<Vec<u32>, String> {
    let mut roots: Vec<u32> = runner.into_iter().collect();
    for word in named.unwrap_or("").split(',').map(str::trim).filter(|w| !w.is_empty()) {
        roots.push(word.parse().map_err(|_| format!("VM_E2E_HOST_ROOTS names pids, not {word:?}"))?);
    }
    Ok(roots)
}

/// This host's processes, every one with its arguments.
pub fn host_processes() -> anyhow::Result<Vec<HostProc>> {
    #[cfg(unix)]
    {
        // `ww`: whole command lines, never cut to a terminal's width (procps and BSD ps alike).
        let out = std::process::Command::new("ps").args(["-A", "-ww", "-o", "pid=", "-o", "ppid=", "-o", "args="]).output()?;
        anyhow::ensure!(out.status.success(), "ps failed: {}", String::from_utf8_lossy(&out.stderr));
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| {
                let mut words = l.split_whitespace();
                let pid = words.next()?.parse().ok()?;
                let ppid = words.next()?.parse().ok()?;
                Some(HostProc { pid, ppid, argv: words.map(str::to_owned).collect() })
            })
            .collect())
    }
    #[cfg(windows)]
    {
        let out = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", "Get-CimInstance Win32_Process | Select-Object ProcessId,ParentProcessId,Name,CommandLine | ConvertTo-Json -Compress"])
            .output()?;
        anyhow::ensure!(out.status.success(), "Get-CimInstance failed: {}", String::from_utf8_lossy(&out.stderr));
        let text = String::from_utf8_lossy(&out.stdout);
        let v: Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))?;
        let rows = match v {
            Value::Array(rows) => rows,
            one => vec![one],
        };
        Ok(rows
            .iter()
            .filter_map(|r| {
                let pid = u32::try_from(r.get("ProcessId")?.as_u64()?).ok()?;
                let ppid = u32::try_from(r.get("ParentProcessId").and_then(Value::as_u64).unwrap_or(0)).ok()?;
                let line = r.get("CommandLine").and_then(Value::as_str).or_else(|| r.get("Name").and_then(Value::as_str)).unwrap_or("");
                Some(HostProc { pid, ppid, argv: windows_words(line) })
            })
            .collect())
    }
}

/// A Windows command line's words, as its C runtime splits them (quotes group, backslashes kept).
#[cfg(windows)]
fn windows_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let (mut word, mut quoted, mut any) = (String::new(), false, false);
    for c in line.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any {
                    words.push(std::mem::take(&mut word));
                    any = false;
                }
            }
            c => {
                word.push(c);
                any = true;
            }
        }
    }
    if any {
        words.push(word);
    }
    words
}

/// The host's harness processes under a runner, looked at as a drive goes: each session's harness
/// is one long-lived process, alive from its start to its stop, so a look between its steps sees it
/// wherever it runs.
pub struct Inventory {
    programs: Vec<String>,
    pub seen: Vec<HostProc>,
    pub looks: usize,
}

impl Inventory {
    pub fn new(programs: &[&str]) -> Self {
        Self { programs: programs.iter().map(|p| (*p).to_owned()).collect(), seen: Vec::new(), looks: 0 }
    }

    /// Looks at the harness processes under `roots` ([`host_roots`]).
    pub fn look(&mut self, roots: &[u32]) -> anyhow::Result<()> {
        let programs: Vec<&str> = self.programs.iter().map(String::as_str).collect();
        for p in harnesses_on_host(&host_processes()?, roots, &programs) {
            if !self.seen.contains(&p) {
                self.seen.push(p);
            }
        }
        self.looks += 1;
        Ok(())
    }
}

/// Where a harness said it ran: `WHERE <kernel name> <boot id>` (`uname -s`, and
/// `/proc/sys/kernel/random/boot_id`, `-` where there's none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Where {
    pub kernel: String,
    pub boot: Option<String>,
}

pub fn where_said(lines: &[String]) -> Option<Where> {
    let rest = last_after(lines, "WHERE ")?;
    let mut w = rest.split_whitespace();
    let kernel = w.next()?.to_owned();
    let boot = w.next().filter(|b| *b != "-").map(str::to_owned);
    Some(Where { kernel, boot })
}

/// This host's boot id, where its kernel has one (Linux).
pub fn host_boot_id() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

/// Ok when the harness ran on a kernel of its own, a microVM's guest: Linux, and on a Linux host a
/// boot other than the host's (a container shares the host's kernel and its boot id; a microVM boots
/// its own). On a macOS or Windows host a Linux kernel is a guest's.
pub fn check_in_guest(said: Option<&Where>, host_os: &str, host_boot: Option<&str>) -> Result<(), String> {
    let w = said.ok_or("the harness never said where it ran (no `WHERE <kernel> <boot id>` line)")?;
    if w.kernel != "Linux" {
        return Err(format!("the harness ran on {}, the host's own kernel, not a Linux guest's", w.kernel));
    }
    if host_os == "linux" {
        let host = host_boot.ok_or("this Linux host says no boot id to tell its kernel from a guest's")?;
        match w.boot.as_deref() {
            None => return Err("the harness said no boot id, so its kernel can't be told from the host's".to_owned()),
            Some(b) if b == host => return Err(format!("the harness ran on the host's own kernel (boot id {b})")),
            Some(_) => {}
        }
    }
    Ok(())
}

/// Ok when kiln says the session runs in a microVM and holds no process of the host: its `runs`
/// (kiln plan step 17) says `in` "microvm" and its `host_pid` null.
pub fn check_no_host_pid(info: &Value) -> Result<(), String> {
    match (info.pointer("/runs/in").and_then(Value::as_str), info.pointer("/runs/host_pid")) {
        (Some("microvm"), Some(Value::Null)) => Ok(()),
        (Some("microvm"), Some(pid)) => Err(format!("kiln says the session holds the host's process {pid}")),
        (Some("microvm"), None) => Err("kiln doesn't say whether the session holds a process of the host (runs.host_pid)".to_owned()),
        (other, _) => Err(format!("kiln says the session runs in {}, not a microVM (runs.in)", other.unwrap_or("nothing it names"))),
    }
}

/// Ok when kiln's record of each start of the session's harness (`starts`, kiln plan step 17) says
/// every one ran in a microVM, so one that ran on the host and ended between two looks is found.
pub fn check_starts(info: &Value) -> Result<(), String> {
    let starts = info.get("starts").and_then(Value::as_array).ok_or("kiln keeps no record of the session's starts (starts)")?;
    if starts.is_empty() {
        return Err("kiln recorded no start of the session's harness".to_owned());
    }
    match starts.iter().find(|s| s.pointer("/runs/in").and_then(Value::as_str) != Some("microvm")) {
        Some(s) => Err(format!("kiln started the session's harness outside a microVM: {s}")),
        None => Ok(()),
    }
}

// ------------------------------------------------------------------------------------------------
// Check 3: the project at its host path
// ------------------------------------------------------------------------------------------------

/// The folder the harness said it works in (`CWD <path>`).
pub fn cwd_said(lines: &[String]) -> Option<String> {
    last_after(lines, "CWD ")
}

/// A host path as a Linux guest sees it plugged in at its host path (Shards D235; kiln plan step 18):
/// the same path on a Unix host; on a Windows host WSL's form of it, a drive at `/mnt/<its letter>`
/// (DrvFs's `automount.root`, drive C: at `/mnt/c`) and `/` between its parts. A verbatim prefix
/// (`\\?\`, which canonicalizing a path on Windows adds) is no part of where it is. A path on no
/// drive (a share's) has no such form and is refused.
pub fn guest_path(host: &str, host_os: &str) -> Result<String, String> {
    if host_os != "windows" {
        return Ok(host.to_owned());
    }
    let path = host.strip_prefix(r"\\?\").unwrap_or(host);
    let mut chars = path.chars();
    match (chars.next(), chars.next(), chars.next()) {
        (Some(drive), Some(':'), Some('\\' | '/')) if drive.is_ascii_alphabetic() => {
            let rest = path.get(3..).unwrap_or("").replace('\\', "/");
            Ok(format!("/mnt/{}/{}", drive.to_ascii_lowercase(), rest.trim_end_matches('/')))
        }
        _ => Err(format!("{host} is on no drive, so a guest sees it at no /mnt/<drive> path (Shards D235)")),
    }
}

/// Ok when kiln has the session in the project's host path and the harness, where it runs, works in
/// that path as its guest sees it ([`guest_path`]; kiln plan step 8: plugged at its host path, not
/// copied under another).
pub fn check_project_path(info_cwd: Option<&str>, harness_cwd: Option<&str>, project: &str, host_os: &str) -> Result<(), String> {
    match info_cwd {
        Some(c) if c == project => {}
        Some(other) => return Err(format!("kiln has the session in {other}, not the project's host path {project}")),
        None => return Err("kiln says no folder for the session".to_owned()),
    }
    let inside = guest_path(project, host_os)?;
    match harness_cwd {
        Some(c) if c == inside => Ok(()),
        Some(other) => Err(format!("the harness works in {other}, not the project's host path as its guest sees it, {inside}")),
        None => Err("the harness never said its folder (no `CWD <path>` line)".to_owned()),
    }
}

/// Ok when the file `name`, which the harness was asked to write in its project with its own name
/// as its words, is at the project's host path with those words: the harness worked in the project
/// itself, not in a copy at the same path.
pub fn check_written(project: &Path, name: &str) -> Result<(), String> {
    match std::fs::read_to_string(project.join(name)) {
        Ok(text) if text.trim() == name => Ok(()),
        Ok(text) => Err(format!("{name} at the project's host path says {:?}, not its name", text.trim())),
        Err(e) => Err(format!("{name}, which the harness was asked to write in its project, isn't at the project's host path: {e}")),
    }
}

// ------------------------------------------------------------------------------------------------
// Check 4: a round trip to kiln's MCP server
// ------------------------------------------------------------------------------------------------

/// Ok, with what shows it, when the harness reached kiln's MCP server from where it runs: a call to
/// one of its tools that came back done (a real harness's, as kiln shows its tool calls: `server`
/// `kiln`), or the stand-in's `MCP_OK <tools> <tool>` with tools listed. `MCP_FAIL <why>` is a reach
/// that failed, and why.
pub fn mcp_round_trip(events: &[Value], lines: &[String]) -> Result<String, String> {
    let call = events.iter().find(|e| {
        e.get("kind").and_then(Value::as_str) == Some("tool")
            && e.get("status").and_then(Value::as_str) == Some("done")
            && (e.get("server").and_then(Value::as_str) == Some("kiln") || e.get("name").and_then(Value::as_str).is_some_and(|n| n.starts_with("mcp__kiln__")))
    });
    if let Some(c) = call {
        return Ok(format!("a call to kiln's {}", c.get("tool_short").or_else(|| c.get("name")).and_then(Value::as_str).unwrap_or("tool")));
    }
    if let Some(ok) = last_after(lines, "MCP_OK ") {
        let tools: u32 = ok.split_whitespace().next().and_then(|n| n.parse().ok()).unwrap_or(0);
        return if tools > 0 { Ok(format!("tools/list answered with {tools} tools")) } else { Err(format!("kiln's MCP server answered with no tools: {ok}")) };
    }
    if let Some(why) = last_after(lines, "MCP_FAIL ") {
        return Err(format!("the harness couldn't reach kiln's MCP server: {why}"));
    }
    Err("the harness made no call to kiln's MCP server".to_owned())
}

// ------------------------------------------------------------------------------------------------
// Check 5: an agent started from inside, placed as an isolated agent's are
// ------------------------------------------------------------------------------------------------

/// The runners that say they start microVMs (`facts.virt.microvm`, from `GET /api/reach/runners`).
pub fn vm_runners(directory: &Value) -> Vec<String> {
    directory
        .get("runners")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|r| r.pointer("/facts/virt/microvm").and_then(Value::as_bool) == Some(true))
        .filter_map(|r| r.get("name").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

/// Ok when `child` (a session as kiln lists it) was started by `parent`, is held in a microVM as an
/// isolated agent's agents are, and runs on a runner that starts them (kiln plan step 32).
pub fn check_child(child: &Value, parent: &str, vm_runners: &[String]) -> Result<(), String> {
    let id = child.get("id").and_then(Value::as_str).unwrap_or("?");
    if child.get("parent").and_then(Value::as_str) != Some(parent) {
        return Err(format!("agent {id} wasn't started by {parent}: its parent is {}", child.get("parent").unwrap_or(&Value::Null)));
    }
    if child.pointer("/isolation/kind").and_then(Value::as_str) != Some("microvm") {
        return Err(format!("agent {id}, started from inside a microVM, isn't held in one: {}", child.get("isolation").unwrap_or(&Value::Null)));
    }
    let machine = child.get("target").and_then(Value::as_str).and_then(|t| t.strip_prefix("agent:"));
    match machine {
        Some(m) if vm_runners.iter().any(|r| r == m) => Ok(()),
        Some(m) => Err(format!("agent {id} runs on {m}, which starts no microVMs (those that do: {vm_runners:?})")),
        None => Err(format!("agent {id} runs on no runner: {}", child.get("target").unwrap_or(&Value::Null))),
    }
}

/// Ok when the audit trail (`GET /api/audit`) records `child`'s start as kiln's `agents.start`
/// writes it, the start that places agents through the runners' admission
/// (`harness::lineage::start_as`, `place_agents`): `… on <its target> for <parent's name> (<the first
/// 8 of the parent's id>)`, a row of its own or a note of the request it came in (`Store::audit`). An
/// agent opened straight through `POST /api/harness/sessions` is recorded without `for`.
pub fn check_child_start(audit: &[Value], child: &Value, parent: &str) -> Result<(), String> {
    let target = child.get("target").and_then(Value::as_str).unwrap_or("");
    let short = parent.get(..8).unwrap_or(parent);
    let (placed, by) = (format!(" on {target} for "), format!("({short})"));
    let found = audit.iter().filter_map(|row| row.get("detail").and_then(Value::as_str)).any(|d| d.split("; ").any(|note| note.contains(&placed) && note.ends_with(&by)));
    if found { Ok(()) } else { Err(format!("the audit trail has no agents.start of an agent on {target} for {short}")) }
}

// ------------------------------------------------------------------------------------------------
// The gate
// ------------------------------------------------------------------------------------------------

/// `VM_E2E=1`: a host that must hold harnesses in microVMs, where one that can't fails rather than
/// skips (as Shards' `this_host_has_its_hypervisor_backend` pins hosts).
pub fn isolation_required() -> bool {
    std::env::var("VM_E2E").is_ok_and(|v| matches!(v.trim(), "1" | "true" | "yes"))
}

/// Why a test skips on runner `machine`, by what it said of microVMs (`facts.virt.microvm`), in its
/// own words where it gives them (`facts.isolation.why_not`, kiln plan step 17's `RunnerFacts`).
pub fn skip_reason(machine: &str, says: Option<bool>, why_not: Option<&str>) -> String {
    match (says, why_not) {
        (Some(true), _) => format!("{machine} starts microVMs"),
        (Some(false), Some(why)) => format!("SKIP: {machine} holds no isolated agents: {why}"),
        (Some(false), None) => format!(
            "SKIP: {machine} starts no microVMs (facts.virt.microvm is false) and says no why: its kiln carries no microVM runtime until kiln plan 01a1cd8e862e7169a074db3d5fa33f3d steps 16 and 17 land (Shards and Hex vendored; the launch seam)"
        ),
        (None, _) => format!("SKIP: {machine} hasn't said whether it starts microVMs (no facts yet)"),
    }
}

// ------------------------------------------------------------------------------------------------
// The fixture: a kiln server and a runner, or ROOT's
// ------------------------------------------------------------------------------------------------

/// A runner process, ended with the test.
pub struct Runner(Child);

impl Runner {
    /// How its process ended, once it has; none while it runs.
    fn ended(&mut self) -> Option<String> {
        self.0.try_wait().ok().flatten().map(|status| status.to_string())
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The server a drive talks to (its HTTP port on 127.0.0.1 and an admin token) and the runner its
/// agents run on (by machine id, as `agent:` targets name it).
pub struct Fixture {
    pub port: u16,
    pub admin: String,
    pub machine: String,
    pub dir: PathBuf,
    /// The runner's pid, whose processes are looked at for harnesses: its own runner's, or ROOT's
    /// (`VM_E2E_RUNNER_PID`).
    pub runner_pid: Option<u32>,
    /// Its own server and runner, or none when it drives ROOT's (`VM_E2E_SERVER`): held so a wait can
    /// tell whether their processes still run ([`Fixture::ended`]).
    _server: Option<Mutex<common::Server>>,
    _runner: Option<Mutex<Runner>>,
    pub log: Option<PathBuf>,
}

impl Fixture {
    pub fn get(&self, path: &str) -> anyhow::Result<Value> {
        let (code, body) = common::http(self.port, "GET", path, &self.admin, None)?;
        anyhow::ensure!(code == 200, "GET {path}: {code} {body}");
        Ok(serde_json::from_str(&body)?)
    }

    pub fn runner_log(&self) -> String {
        self.log.as_ref().map(|l| common::tail(l, 60)).unwrap_or_default()
    }

    /// One of its own processes ended: which, and how. ROOT's runner (`VM_E2E_RUNNER_PID`), which it
    /// didn't start, is looked at by its pid where kiln can tell a pid runs (Unix: `os::pid_alive`).
    pub fn ended(&self) -> Option<SetupStuck> {
        let server = self._server.as_ref().and_then(|s| s.lock().unwrap_or_else(PoisonError::into_inner).ended());
        if let Some(how) = server {
            return Some(SetupStuck::Exited { party: Party::Server, how });
        }
        let runner = self._runner.as_ref().and_then(|r| r.lock().unwrap_or_else(PoisonError::into_inner).ended());
        if let Some(how) = runner {
            return Some(SetupStuck::Exited { party: Party::Runner, how });
        }
        #[cfg(unix)]
        if self._runner.is_none()
            && let Some(pid) = self.runner_pid
            && !kiln::os::pid_alive(pid)
        {
            return Some(SetupStuck::Exited { party: Party::Runner, how: format!("its pid {pid} runs no more") });
        }
        None
    }

    /// Its runner's whole log: its own runner's, or ROOT's runner's (`VM_E2E_RUNNER_LOG`).
    pub fn runner_log_whole(&self) -> anyhow::Result<String> {
        let path = self.log.clone().or_else(|| std::env::var_os("VM_E2E_RUNNER_LOG").map(PathBuf::from)).ok_or_else(|| anyhow::anyhow!("name ROOT's runner's log in VM_E2E_RUNNER_LOG (tests/vm_e2e/README.md)"))?;
        std::fs::read_to_string(&path).map_err(|e| anyhow::anyhow!("the runner's log {}: {e}", path.display()))
    }
}

// ------------------------------------------------------------------------------------------------
// The fixture's setup waits: its server's and runner's liveness and kiln's word, never a clock
// (docs/research/s35-every-harness/waits/README.md)
// ------------------------------------------------------------------------------------------------

/// A party of a fixture's setup, as its waits follow it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Party {
    Server,
    Runner,
}

/// Why a setup wait can't get its answer: each a state no waiting changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupStuck {
    /// A process of the fixture's own ended, and how.
    Exited { party: Party, how: String },
    /// The server's final word on what was asked: a refusal (`code`, its words) that asking again
    /// doesn't change.
    Refused { code: u16, body: String },
}

impl std::fmt::Display for SetupStuck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exited { party, how } => write!(f, "the {party:?} process ended ({how}): nothing of it answers again"),
            Self::Refused { code, body } => write!(f, "the server refused it ({code} {body}): asking again changes nothing"),
        }
    }
}

/// What one look of a setup wait says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Setup<T> {
    /// Its answer.
    Ready(T),
    /// It may still come: its parties live.
    Wait,
    /// It won't.
    Refuse(SetupStuck),
}

/// A setup wait's refusal: what it waited for, why it ended (typed), and its runner's log.
#[derive(Debug)]
pub struct SetupRefused {
    pub what: String,
    pub stuck: SetupStuck,
    pub evidence: String,
}

impl std::fmt::Display for SetupRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}\nits runner's log, its last lines:\n{}", self.what, self.stuck, self.evidence)
    }
}

impl std::error::Error for SetupRefused {}

/// Follows a setup wait: `look` until it gives its answer, refused at once, typed, where it says none
/// can come or `ended` says a party's process ended. Between looks it waits on `wake`, kiln's next
/// change notification, given how long the look took. A look that couldn't reach the server (an
/// `Err`) is looked again while the parties live. No time bound: a setup wait is its parties' cold
/// start, before anything of theirs has answered.
pub fn setup<T>(ended: impl Fn() -> Option<SetupStuck>, mut look: impl FnMut() -> anyhow::Result<Setup<T>>, refuse: impl Fn(SetupStuck) -> anyhow::Error, mut wake: impl FnMut(Duration)) -> anyhow::Result<T> {
    loop {
        if let Some(stuck) = ended() {
            return Err(refuse(stuck));
        }
        let looked = Instant::now();
        match look() {
            Ok(Setup::Ready(answer)) => return Ok(answer),
            Ok(Setup::Refuse(stuck)) => return Err(refuse(stuck)),
            Ok(Setup::Wait) | Err(_) => wake(looked.elapsed()),
        }
    }
}

/// [`setup`] on fixture `f`'s parties, woken by kiln's events socket, refused with its runner's log.
fn setup_on<T>(f: &Fixture, what: &str, look: impl FnMut() -> anyhow::Result<Setup<T>>) -> anyhow::Result<T> {
    let mut wake = Wake::events(f);
    setup(|| f.ended(), look, |stuck| anyhow::Error::new(SetupRefused { what: what.to_owned(), stuck, evidence: f.runner_log() }), |took| {
        wake.next(None, took);
    })
}

// ------------------------------------------------------------------------------------------------
// What a wait wakes on: kiln's own change notifications (R05; the waits note)
// ------------------------------------------------------------------------------------------------

/// kiln's change notifications a wait wakes on: a socket kiln pushes its changes down, a session's
/// own ([`Wake::session`]: what its viewers are sent, every update of its copy) or the events socket
/// ([`Wake::events`]: the hub's events, runners' records among them), read a frame at a time.
#[derive(Debug)]
pub struct Wake {
    port: u16,
    admin: String,
    path: String,
    /// The session's socket sends what the session has first, up to `synced`: what's there, not a change.
    snapshot: bool,
    sock: Option<std::net::TcpStream>,
}

impl Wake {
    /// Session `id`'s own socket, its viewers'.
    pub fn session(f: &Fixture, id: &str) -> Self {
        Self { port: f.port, admin: f.admin.clone(), path: format!("/api/harness/sessions/{id}/ws"), snapshot: true, sock: None }
    }

    /// kiln's events socket (`GET /api/events`, each event as it comes).
    pub fn events(f: &Fixture) -> Self {
        Self { port: f.port, admin: f.admin.clone(), path: "/api/events".to_owned(), snapshot: false, sock: None }
    }

    /// Waits for kiln's next change: until a frame comes down the socket, or `within` passes (none: no
    /// bound). Frames that follow it within `settle` (how long a look takes) are read with it, so looks
    /// never outrun kiln's answers. The socket is opened as it's first waited on, and again after it
    /// ended; that wait returns at once, so a change made while it opened is looked at. An opening
    /// that fails pauses as long as it took. What it read, each frame's JSON, is its answer (a session's
    /// admission's word on a message among them: [`Watch::heard`]).
    pub fn next(&mut self, within: Option<Duration>, settle: Duration) -> Vec<Value> {
        let mut said = Vec::new();
        if self.sock.is_none() {
            let began = Instant::now();
            match self.open() {
                Ok(s) => self.sock = Some(s),
                Err(_) => std::thread::sleep(began.elapsed()),
            }
            return said;
        }
        if within.is_some_and(|w| w.is_zero()) {
            return said;
        }
        let Some(s) = self.sock.as_mut() else { return said };
        // A frame within `t` (none: no bound), each read with its own bound: a frame that began is
        // read whole, with none.
        let mut read = |s: &mut std::net::TcpStream, t: Option<Duration>| -> anyhow::Result<bool> {
            s.set_read_timeout(t)?;
            let Some(payload) = frame_or_quiet(s)? else { return Ok(false) };
            if let Ok(v) = serde_json::from_slice::<Value>(&payload) {
                said.push(v);
            }
            Ok(true)
        };
        let ended = match read(s, within) {
            Ok(true) => {
                !settle.is_zero()
                    && loop {
                        match read(s, Some(settle)) {
                            Ok(true) => {}
                            Ok(false) => break false,
                            Err(_) => break true,
                        }
                    }
            }
            Ok(false) => false,
            Err(_) => true,
        };
        if ended {
            self.sock = None;
        }
        said
    }

    fn open(&self) -> anyhow::Result<std::net::TcpStream> {
        let mut s = common::agents::socket(self.port, &self.admin, &self.path)?;
        if self.snapshot {
            common::agents::until_synced(&mut s)?;
        }
        Ok(s)
    }
}

/// One WebSocket frame off `s`, the server's (unmasked): its payload where one came; none where none
/// began within its read timeout, so nothing of one was read and the socket stays whole; an error: it
/// ended or broke. A frame that began is read whole, its sender writing it whole.
pub fn frame_or_quiet(s: &mut std::net::TcpStream) -> anyhow::Result<Option<Vec<u8>>> {
    let mut first = [0u8; 1];
    match s.read(&mut first) {
        Ok(0) => anyhow::bail!("the socket ended"),
        Ok(_) => {}
        Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    s.set_read_timeout(None)?;
    let second = common::agents::take(s, 1)?.first().copied().unwrap_or_default();
    let len = match second & 0x7f {
        126 => common::agents::take(s, 2)?.iter().fold(0usize, |a, b| (a << 8) | usize::from(*b)),
        127 => common::agents::take(s, 8)?.iter().fold(0usize, |a, b| (a << 8) | usize::from(*b)),
        n => usize::from(n),
    };
    Ok(Some(common::agents::take(s, len)?))
}

/// What a server's answer other than the one looked for says: a refusal at its word (a client error,
/// 4xx), or its own trouble (5xx), looked past while it lives.
pub fn server_said<T>(code: u16, body: &str) -> Setup<T> {
    if (400..500).contains(&code) { Setup::Refuse(SetupStuck::Refused { code, body: body.to_owned() }) } else { Setup::Wait }
}

/// Whether kiln's list of runners (`GET /api/agents`) has `machine` connected; listed but not
/// connected, it's between connections.
pub fn connected(agents: &Value, machine: &str) -> bool {
    let list = agents.as_array().or_else(|| agents.get("agents").and_then(Value::as_array));
    list.into_iter().flatten().any(|a| a.get("name").and_then(Value::as_str) == Some(machine) && a.get("connected").and_then(Value::as_bool) == Some(true))
}

/// What a runner's entry in kiln's reach (`GET /api/reach/runners`) says of microVMs: once its facts
/// came (they always say `virt.microvm`, `kiln_ci::facts::Virt`), whether it starts them and why not;
/// before, nothing yet.
pub fn microvms_said(entry: Option<&Value>) -> Setup<(bool, Option<String>)> {
    match entry.and_then(|r| r.pointer("/facts/virt/microvm")).and_then(Value::as_bool) {
        Some(says) => Setup::Ready((says, entry.and_then(|r| r.pointer("/facts/isolation/why_not")).and_then(Value::as_str).map(str::to_owned))),
        None => Setup::Wait,
    }
}

/// What the server's answer to a start (`POST /api/harness/sessions`) says: its session's id; a
/// refusal given while the runner was connected before and after it, kiln's word on what was asked;
/// else, the runner between connections, one to ask again.
pub fn opened(code: u16, body: &str, connected_throughout: bool) -> Setup<String> {
    let id = || serde_json::from_str::<Value>(body).ok().and_then(|v| v.pointer("/0/id").and_then(Value::as_str).map(str::to_owned));
    match (code, connected_throughout) {
        (200, _) => id().map_or_else(|| Setup::Refuse(SetupStuck::Refused { code, body: body.to_owned() }), Setup::Ready),
        (_, true) => server_said(code, body),
        (_, false) => Setup::Wait,
    }
}

/// Whether kiln says a runner applied its headroom setting (`GET /api/runners/<machine>/headroom`).
pub fn headroom_applied(body: &Value) -> Setup<()> {
    if body.get("applied").and_then(Value::as_bool) == Some(true) { Setup::Ready(()) } else { Setup::Wait }
}

/// A server of its own under `dir` and a runner `name` of it, connected, `env` in the runner's
/// environment (and so its harnesses'); or, with `VM_E2E_SERVER` (`http://127.0.0.1:<port>`),
/// `VM_E2E_TOKEN`, `VM_E2E_MACHINE` and `VM_E2E_RUNNER_PID`, ROOT's server and runner: one with the
/// sign-ins and credentials real turns need.
pub fn up(dir: &Path, name: &str, env: &[(&str, &str)]) -> anyhow::Result<Fixture> {
    if let Ok(url) = std::env::var("VM_E2E_SERVER") {
        let port = url.trim().strip_prefix("http://127.0.0.1:").and_then(|p| p.trim_end_matches('/').parse().ok()).ok_or_else(|| anyhow::anyhow!("VM_E2E_SERVER is http://127.0.0.1:<port>, not {url}"))?;
        let admin = std::env::var("VM_E2E_TOKEN").map_err(|_| anyhow::anyhow!("VM_E2E_SERVER needs VM_E2E_TOKEN, its admin token"))?;
        let machine = std::env::var("VM_E2E_MACHINE").map_err(|_| anyhow::anyhow!("VM_E2E_SERVER needs VM_E2E_MACHINE, its runner's machine id"))?;
        let runner_pid = std::env::var("VM_E2E_RUNNER_PID").ok().and_then(|p| p.trim().parse().ok());
        return Ok(Fixture { port, admin, machine, dir: dir.to_path_buf(), runner_pid, _server: None, _runner: None, log: None });
    }
    up_own(dir, name, env)
}

/// A server of its own under `dir` and a runner `name` of it, connected, whatever `VM_E2E_SERVER`
/// says: a run that reads or writes what its runner keeps on the machine (its folder, `<dir>/<name>-state`).
pub fn up_own(dir: &Path, name: &str, env: &[(&str, &str)]) -> anyhow::Result<Fixture> {
    let server = common::start(dir, &common::Options::default())?;
    let (runner, log) = runner(dir, server.port, name, env)?;
    let f = Fixture { port: server.port, admin: server.admin.clone(), machine: common::machine(name), dir: dir.to_path_buf(), runner_pid: Some(runner.0.id()), _server: Some(Mutex::new(server)), _runner: Some(Mutex::new(runner)), log: Some(log) };
    setup_on(&f, &format!("runner {name} connecting"), || {
        let (code, body) = common::http(f.port, "GET", "/api/agents", &f.admin, None)?;
        if code != 200 {
            return Ok(server_said(code, &body));
        }
        Ok(if connected(&serde_json::from_str(&body)?, &f.machine) { Setup::Ready(()) } else { Setup::Wait })
    })?;
    // Its headroom off: a test's runner shares a machine the rest of the tests (and, on a shared host,
    // everyone else) keep busy, past which kiln holds a queued message in its own queue for room
    // (`harness::claude`'s `write_send`): the drive's turns waited there, not at work (adacachy, load
    // ~50 on 32 cores), which isn't what it checks (as `agent_limit.rs` turns its runner's off).
    headroom_off(&f, name)?;
    Ok(f)
}

/// Turns the fixture's runner `name`'s headroom off (docs/headroom.md) and waits until it says it
/// applied that, following its parties ([`setup`]), as `common::agents::headroom_off` does by a deadline.
fn headroom_off(f: &Fixture, name: &str) -> anyhow::Result<()> {
    let path = format!("/api/runners/{}/headroom", common::machine(name));
    let what = format!("{name}'s headroom off");
    setup_on(f, &what, || {
        let (code, body) = common::http(f.port, "PUT", &path, &f.admin, Some(&json!({"enabled": false})))?;
        Ok(if code == 200 { Setup::Ready(()) } else { server_said(code, &body) })
    })?;
    setup_on(f, &what, || {
        let (code, body) = common::http(f.port, "GET", &path, &f.admin, None)?;
        if code != 200 {
            return Ok(server_said(code, &body));
        }
        Ok(headroom_applied(&serde_json::from_str(&body)?))
    })
}

/// The folder runner `name` of a fixture under `dir` keeps its state in (`--state-dir`): its own folder
/// (`kiln::upgrade::own_dir`, no instance of its own).
pub fn own_state(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}-state"))
}

/// `kiln agent` for the server at `port`, as `common::agents::runner_env` starts one, on any OS: a
/// machine and a home of its own (on Windows its profile too, as `common::Fleet` gives one).
fn runner(dir: &Path, port: u16, name: &str, env: &[(&str, &str)]) -> anyhow::Result<(Runner, PathBuf)> {
    let join = std::fs::read_to_string(dir.join("data").join("join.token"))?.trim().to_owned();
    let home = dir.join(format!("{name}-home"));
    std::fs::create_dir_all(&home)?;
    let log_path = dir.join(format!("{name}.log"));
    let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
    let mut cmd = common::kiln();
    cmd.args(["agent", "--server", &format!("http://127.0.0.1:{port}"), "--token", &join, "--name", name])
        .arg("--work-dir")
        .arg(dir.join(format!("{name}-work")))
        .arg("--state-dir")
        .arg(own_state(dir, name))
        .env("KILN_MACHINE_ID", name)
        .env("HOME", &home)
        .env_remove("GIT_CONFIG_GLOBAL")
        .env("NO_COLOR", "1")
        .env(common::WARM_FLOOR.0, common::WARM_FLOOR.1);
    #[cfg(windows)]
    cmd.env("USERPROFILE", &home).env("APPDATA", home.join("AppData").join("Roaming")).env("LOCALAPPDATA", home.join("AppData").join("Local"));
    let child = cmd.envs(env.iter().copied()).stdin(Stdio::null()).stdout(Stdio::from(log.try_clone()?)).stderr(Stdio::from(log)).spawn()?;
    Ok((Runner(child), log_path))
}

/// The gate: whether the fixture's runner starts microVMs. Ok(false) after saying why it skips;
/// an error where `VM_E2E=1` requires it.
pub fn isolation_ready(f: &Fixture) -> anyhow::Result<bool> {
    let (says, why_not) = setup_on(f, &format!("{}'s facts on microVMs", f.machine), || {
        let (code, body) = common::http(f.port, "GET", "/api/reach/runners", &f.admin, None)?;
        if code != 200 {
            return Ok(server_said(code, &body));
        }
        let reach: Value = serde_json::from_str(&body)?;
        let entry = reach.get("runners").and_then(Value::as_array).and_then(|rs| rs.iter().find(|r| r.get("name").and_then(Value::as_str) == Some(f.machine.as_str())));
        Ok(microvms_said(entry))
    })?;
    if says {
        return Ok(true);
    }
    let why = skip_reason(&f.machine, Some(says), why_not.as_deref());
    anyhow::ensure!(!isolation_required(), "{why} (VM_E2E=1 requires microVMs here)");
    eprintln!("{why}");
    Ok(false)
}

// ------------------------------------------------------------------------------------------------
// A session, through kiln's API
// ------------------------------------------------------------------------------------------------

/// Starts a session of `spec` on the fixture's runner: its id. Asked again while the runner is
/// between connections ([`opened`]), following the fixture's parties ([`setup`]).
pub fn open(f: &Fixture, spec: &Value) -> anyhow::Result<String> {
    let body = json!({"target": format!("agent:{}", f.machine), "spec": spec});
    let up = || -> anyhow::Result<bool> {
        let (code, text) = common::http(f.port, "GET", "/api/agents", &f.admin, None)?;
        Ok(code == 200 && connected(&serde_json::from_str(&text)?, &f.machine))
    };
    setup_on(f, &format!("starting {spec}"), || {
        let before = up()?;
        // Longer than the server waits for its runner's answer (`hub::COMMAND_WITHIN`).
        let (code, text) = common::http_within(f.port, "POST", "/api/harness/sessions", &f.admin, Some(&body), Duration::from_secs(75))?;
        let throughout = code != 200 && before && up()?;
        Ok(opened(code, &text, throughout))
    })
}

/// The session as kiln lists it.
pub fn info(f: &Fixture, id: &str) -> anyhow::Result<Value> {
    f.get(&format!("/api/harness/sessions/{id}"))
}

/// Its timeline's events, oldest first.
pub fn events(f: &Fixture, id: &str) -> anyhow::Result<Vec<Value>> {
    let v = f.get(&format!("/api/harness/sessions/{id}/events?limit=1000"))?;
    Ok(match v {
        Value::Array(a) => a,
        other => other.get("events").and_then(Value::as_array).cloned().unwrap_or_default(),
    })
}

pub fn command(f: &Fixture, id: &str, cmd: &Value) -> anyhow::Result<()> {
    let (code, body) = common::http_within(f.port, "POST", &format!("/api/harness/sessions/{id}/command"), &f.admin, Some(cmd), Duration::from_secs(75))?;
    anyhow::ensure!(matches!(code, 200 | 202 | 204), "{cmd}: {code} {body}");
    Ok(())
}

pub fn status(i: &Value) -> &str {
    i.get("status").and_then(Value::as_str).unwrap_or("")
}

/// Where kiln's message `client` is in the timeline (the person's words as kiln delivered them).
pub fn delivered_at(events: &[Value], client: &str) -> Option<usize> {
    events.iter().position(|e| e.get("kind").and_then(Value::as_str) == Some("user") && e.get("client_id").and_then(Value::as_str) == Some(client))
}

/// What the harness said after kiln delivered message `client`.
pub fn said_after(events: &[Value], client: &str) -> Vec<String> {
    delivered_at(events, client).map(|at| said(events.get(at + 1..).unwrap_or_default())).unwrap_or_default()
}

// ------------------------------------------------------------------------------------------------
// Waiting on a session: its harness's liveness and progress, never a clock (kiln plan
// 01a1cd8e862e7169a074db3d5fa33f3d step 35; docs/research/s35-every-harness/waits/README.md)
// ------------------------------------------------------------------------------------------------

/// What a wait asked of its harness: how its answer is told, and whether an earlier answer bounds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asked<'a> {
    /// The session's opening prompt, which kiln sends under an id made of the session's own
    /// (`harness::session`'s `first-prompt`).
    Opening,
    /// A message the drive sent, by the client id it sent it under or by words of its own it carries
    /// (one sent through kiln's MCP tools, which take no client id).
    Message(&'a str),
    /// A message steered into a turn the drive asked to be long ([`Ask::steered`]), told as an
    /// [`Asked::Message`] is. No earlier answer bounds a turn asked to take longer: it waits as a
    /// drive's first wait does, on its harness's liveness and progress alone.
    Steered(&'a str),
    /// The turn under way stopped (an interrupt), which idle answers. `said`: the lines the session
    /// had said as the stop was taken, where its turn says no more (the stand-in's held turn,
    /// [`stub_stopped_turn`]); none: not counted.
    Stop { said: Option<usize> },
    /// Something kiln does of the session rather than its harness (listing it no more once it was
    /// ended, listing an agent it started): only its progress and the drive's window judge it.
    Kiln,
}

/// Why a session can't answer a wait, as kiln's own word on it says: each a state kiln leaves only at
/// a person's word, which a drive never gives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stuck {
    /// No runner holds its harness (`Status::Crashed`): only a person's resume takes it up.
    Crashed,
    /// Its startup was refused while its process lives (`exited` with a process still running: an
    /// ACP agent's refused startup, CP-T3.2): a sign-in or a restart takes it on.
    Refused { error: Option<String> },
    /// Its process ended and kiln starts it no more: it never started, or kiln's word after its end
    /// set no restart (`ExitInfo::again_at`, which kiln sets where it starts it again by itself).
    Ended { code: Option<i64>, error: Option<String> },
    /// Idle: what it was asked delivered and its delivery over, and nothing else on its way.
    Answered,
    /// It waits for a person's answer: its approvals, by title.
    Asks(Vec<String>),
    /// It said more after its stop was taken: those lines.
    NotStopped(Vec<String>),
}

impl std::fmt::Display for Stuck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Crashed => write!(f, "no runner holds its harness (crashed): only a person's resume takes it up"),
            Self::Refused { error } => write!(f, "its startup was refused while its process lives ({}): a sign-in or a restart takes it on", error.as_deref().unwrap_or("kiln gave no reason")),
            Self::Ended { code, error } => {
                let code = code.map_or_else(|| "none".to_owned(), |c| c.to_string());
                write!(f, "its process ended (code {code}, {}) and kiln starts it no more", error.as_deref().unwrap_or("no error"))
            }
            Self::Answered => write!(f, "it is idle, what it was asked delivered and its turn over, with nothing queued or under way: no answer is coming"),
            Self::Asks(titles) => write!(f, "it waits for a person's answer, which a drive never gives: {titles:?}"),
            Self::NotStopped(lines) => write!(f, "it went on after its stop was taken: {lines:?}"),
        }
    }
}

/// Why a wait ended without its answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stall {
    /// The session can't answer it.
    Stuck(Stuck),
    /// No progress for a whole window as long as the slowest answer its harness gave in this drive.
    Quiet { quiet: Duration, slowest: Duration },
    /// Its message waits in kiln's queue (its delivery `queued`, or in the queue) on an idle session for
    /// a whole window, and no cause kiln states applies to it ([`holds`]): kiln never hands it over.
    DeliveryStuck { message: String, quiet: Duration, slowest: Duration },
}

impl std::fmt::Display for Stall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stuck(s) => std::fmt::Display::fmt(s, f),
            Self::Quiet { quiet, slowest } => write!(f, "no progress for {quiet:?}, a whole window as long as its slowest answer in this drive ({slowest:?})"),
            Self::DeliveryStuck { message, quiet, slowest } => write!(
                f,
                "delivery stuck: {message} waits in kiln's queue on an idle session, and no cause kiln states applies to it (admission's own wait on it, a room wait, a pause, a freeze), for {quiet:?}, a whole window as long as its slowest answer in this drive ({slowest:?})"
            ),
        }
    }
}

/// A wait's refusal: what it waited for, why it ended (typed), and kiln's word on the session then
/// ([`told`]).
#[derive(Debug)]
pub struct Refused {
    pub what: String,
    pub stall: Stall,
    pub evidence: String,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}\n{}", self.what, self.stall, self.evidence)
    }
}

impl std::error::Error for Refused {}

/// What a drive has learned of its harness's answers: the slowest so far, each one whole wait from the
/// drive's ask to its answer; none before its first, the drive's cold start. The one value its waits
/// are bounded by: nothing is picked, seeded or carried over from another run.
#[derive(Debug, Default)]
pub struct Pace {
    slowest: Option<Duration>,
    /// Each session's feed ([`Wake::session`]), held from its first wait until it's ended: what kiln
    /// says of a message the drive sends between waits (admission's word on it) is heard by the next.
    feeds: std::collections::BTreeMap<String, Wake>,
}

/// What a look at a session tells its wait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Its condition holds.
    Done,
    /// Its answer may still come.
    Wait,
    /// kiln holds what it was asked in its custody for a cause it states of it ([`holds`]), said: a
    /// hold, which the window doesn't run through (R03: a host's hold is said as held, and the wait goes
    /// on).
    Held(String),
    /// It won't.
    Refuse(Stall),
}

/// kiln's word on a session whose change is progress: its harness's and kiln's work on it. Not
/// `last_activity`, which every state of it kiln is sent moves, the same state again included.
const MOVES: [&str; 18] = [
    "status", "queued", "doing", "activity", "turn_started", "last_turn", "exit", "runs", "starts", "approvals", "tasks_running", "subagents", "pause", "frozen", "room_wait", "admission", "harness_session", "mode",
];

/// kiln's word on a session that a wait's evidence shows: what moves, what holds it, and when kiln
/// last heard of it.
const TOLD_FIELDS: [&str; 21] = [
    "status", "queued", "doing", "activity", "turn_started", "last_turn", "exit", "runs", "starts", "approvals", "tasks_running", "subagents", "pause", "frozen", "room_wait", "admission", "harness_session", "mode", "held", "refused", "last_activity",
];

/// How much of each event's words a wait's evidence shows: the first 80 characters, as ROOT asked of
/// it (2026-11-13), whole where shorter.
const TOLD: usize = 80;

/// kiln's admission's word on each message it was offered, as the session's feed says it: each
/// `mail_flow` frame (`harness::admission::FlowEvent`, `wire::ui::SessionFrame::MailFlow`) with the
/// message's publication, whose id is the message's. The latest word on each message, by its id. The
/// admission capacity kiln's word on a session carries names no message and states no cause.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Admission {
    said: std::collections::BTreeMap<String, Value>,
}

impl Admission {
    /// A frame of the session's feed: a `mail_flow` frame naming its message's publication is
    /// admission's latest word on that message; any other frame says nothing of admission.
    pub fn heard(&mut self, frame: &Value) {
        if frame.get("t").and_then(Value::as_str) != Some("mail_flow") {
            return;
        }
        if let (Some(id), Some(event)) = (frame.pointer("/publication/id").and_then(Value::as_str), frame.get("event")) {
            self.said.insert(id.to_owned(), event.clone());
        }
    }

    /// Admission's own wait cause for message `client` (`harness::admission::WaitCause`), where its
    /// latest word on it is a wait. None: it said nothing of it, or not a wait.
    pub fn waits(&self, client: &str) -> Option<&str> {
        let said = self.said.get(client)?;
        if said.get("event").and_then(Value::as_str) != Some("waiting") {
            return None;
        }
        said.get("cause").and_then(Value::as_str)
    }
}

/// One wait's memory of its looks at its session ([`Watch::look`]).
#[derive(Debug, Clone)]
pub struct Watch<'a> {
    asked: Asked<'a>,
    window: Option<Duration>,
    /// What of the session moves ([`moving`]), as last seen.
    seen: Option<Value>,
    /// When it last moved, from the wait's start: the drive's ask, at zero.
    moved: Duration,
    /// An end of its harness's that kiln had set no restart for yet: its `exit.at`, and kiln's word's
    /// time (`last_activity`) when it was seen.
    end: Option<(i64, i64)>,
    /// kiln's admission's word on each message, from the session's feed ([`Watch::heard`]).
    admission: Admission,
}

impl<'a> Watch<'a> {
    pub fn new(asked: Asked<'a>, window: Option<Duration>) -> Self {
        Self { asked, window, seen: None, moved: Duration::ZERO, end: None, admission: Admission::default() }
    }

    /// A frame of the session's feed, as the wait's socket read it ([`Wake::next`]): admission's word
    /// on a message, where it is one ([`Admission::heard`]).
    pub fn heard(&mut self, frame: &Value) {
        self.admission.heard(frame);
    }

    /// The message the wait asked for an answer to, by what [`is_message`] takes: none for a stop and
    /// for kiln's own work.
    fn message(&self, info: &Value) -> Option<String> {
        match self.asked {
            Asked::Opening => Some(opening_id(info.get("id").and_then(Value::as_str).unwrap_or_default())),
            Asked::Message(m) | Asked::Steered(m) => Some(m.to_owned()),
            Asked::Stop { .. } | Asked::Kiln => None,
        }
    }

    /// Whether a look `at` after the wait began needs the session's state (each of its messages'
    /// delivery, which only its viewers are sent): kiln's word says it may have answered what it was
    /// asked ([`may_have_answered`]), or its window runs out and only the state says whether kiln holds it
    /// ([`holds`]).
    pub fn wants_state(&self, info: &Value, events: &[Value], at: Duration) -> bool {
        self.message(info).is_some_and(|m| may_have_answered(info, events, &m).is_some() || self.window.is_some_and(|w| at.saturating_sub(self.moved) > w))
    }

    /// A look at kiln's word on the session (`info`), its timeline (`events`) and, where
    /// [`Watch::wants_state`] asked for it, its state (`state`), `at` after the wait began, `done` its
    /// caller's condition of them: done; refused at once where kiln's word says no answer can come;
    /// where a window is known and nothing that moves moved for a whole one, held where a cause kiln
    /// states applies to its message ([`holds`]), else refused: delivery stuck where its message waits in
    /// kiln's queue on an idle session, quiet otherwise; else it waits on.
    pub fn look(&mut self, info: &Value, events: &[Value], state: Option<&Value>, done: bool, at: Duration) -> Verdict {
        if done {
            return Verdict::Done;
        }
        if let Some(stuck) = self.stuck(info, events, state) {
            return Verdict::Refuse(Stall::Stuck(stuck));
        }
        let now = moving(info, events);
        if self.seen.as_ref() != Some(&now) {
            if self.seen.is_some() {
                self.moved = at;
            }
            self.seen = Some(now);
        }
        let quiet = at.saturating_sub(self.moved);
        let Some(slowest) = self.window.filter(|w| quiet > *w) else { return Verdict::Wait };
        let message = self.message(info);
        // kiln holding it, for a cause it states of it, is kiln's progress on it: the window starts again.
        if let Some(why) = message.as_deref().and_then(|m| holds(info, events, state, &self.admission, m)) {
            self.moved = at;
            return Verdict::Held(why);
        }
        match message.map(|m| client_of(events, &m)).filter(|client| status(info) == "idle" && state.is_some_and(|s| in_queue(s, client))) {
            Some(message) => Verdict::Refuse(Stall::DeliveryStuck { message, quiet, slowest }),
            None => Verdict::Refuse(Stall::Quiet { quiet, slowest }),
        }
    }

    /// How long it may yet go without progress, `at` after it began, before a look refuses it: none,
    /// where no window bounds it.
    pub fn left(&self, at: Duration) -> Option<Duration> {
        self.window.map(|w| self.moved.saturating_add(w).saturating_sub(at))
    }

    fn stuck(&mut self, info: &Value, events: &[Value], state: Option<&Value>) -> Option<Stuck> {
        if self.asked == Asked::Kiln {
            return None;
        }
        if let Some(ended) = self.ended(info) {
            return Some(ended);
        }
        let asks: Vec<String> = info.get("approvals").and_then(Value::as_array).into_iter().flatten().map(|a| a.get("title").and_then(Value::as_str).unwrap_or_default().to_owned()).collect();
        if !asks.is_empty() {
            return Some(Stuck::Asks(asks));
        }
        if let Asked::Stop { said: Some(n) } = self.asked {
            return said(events).get(n..).filter(|more| !more.is_empty()).map(|more| Stuck::NotStopped(more.to_vec()));
        }
        let client = self.message(info).and_then(|m| may_have_answered(info, events, &m))?;
        state.is_some_and(|s| answered_in(s, &client)).then_some(Stuck::Answered)
    }

    /// Its harness gone for good, by kiln's word: no runner holds it; its startup refused while its
    /// process lives; or its process ended with no restart set, where it never started (kiln starts
    /// again only what started) or kiln has said a word of it since its end and set none. kiln sends
    /// an end and its decision on it one after the other, so an end with no restart yet is waited on
    /// until kiln's next word.
    fn ended(&mut self, info: &Value) -> Option<Stuck> {
        match status(info) {
            "crashed" => return Some(Stuck::Crashed),
            "exited" => {}
            _ => {
                self.end = None;
                return None;
            }
        }
        let exit = info.get("exit").unwrap_or(&Value::Null);
        let error = exit.get("error").and_then(Value::as_str).map(str::to_owned);
        if present(info, "runs") {
            return Some(Stuck::Refused { error });
        }
        if present(exit, "again_at") {
            self.end = None;
            return None;
        }
        let code = exit.get("code").and_then(Value::as_i64);
        if info.get("starts").and_then(Value::as_array).is_none_or(Vec::is_empty) {
            return Some(Stuck::Ended { code, error });
        }
        let at = exit.get("at").and_then(Value::as_i64).unwrap_or_default();
        let word = info.get("last_activity").and_then(Value::as_i64).unwrap_or_default();
        match self.end {
            Some((seen, then)) if seen == at => (word > then).then_some(Stuck::Ended { code, error }),
            _ => {
                self.end = Some((at, word));
                None
            }
        }
    }
}

/// Whether `v` has `key`, not null.
fn present(v: &Value, key: &str) -> bool {
    v.get(key).is_some_and(|x| !x.is_null())
}

/// The id kiln sends session `session`'s opening prompt under (`harness::session`: made once, of the
/// session's own).
pub fn opening_id(session: &str) -> String {
    kiln::util::derive_id(&["first-prompt", session])
}

/// Whether timeline event `e` is message `m`: a person's words under client id `m`, or carrying `m`.
fn is_message(e: &Value, m: &str) -> bool {
    !m.is_empty() && e.get("kind").and_then(Value::as_str) == Some("user") && (e.get("client_id").and_then(Value::as_str) == Some(m) || e.get("text").and_then(Value::as_str).is_some_and(|t| t.contains(m)))
}

/// Where kiln's word says the session may have answered message `m` with nothing more to come: idle,
/// `m` in its timeline, nothing queued, under way, waiting for room or paused, and its last turn not
/// one kiln may keep going (`cut`, `limit`). `m`'s client id then, which its delivery is kept under:
/// whose turn ended only the delivery says ([`answered_in`]), as a harness that queues a message
/// itself (Claude Code's, written mid-turn) is idle with it still to answer.
fn may_have_answered(info: &Value, events: &[Value], m: &str) -> Option<String> {
    let count = |key: &str| info.get(key).and_then(Value::as_u64).unwrap_or_default();
    let live = info.get("subagents").and_then(Value::as_array).into_iter().flatten().any(|t| t.get("status").and_then(Value::as_str).is_some_and(kiln::harness::model::live_task));
    let kept = info.get("last_turn").and_then(|t| t.get("how")).and_then(Value::as_str).is_some_and(|how| matches!(how, "cut" | "limit"));
    let calm = status(info) == "idle" && count("queued") == 0 && count("tasks_running") == 0 && !live && !kept && !["pause", "room_wait", "frozen"].iter().any(|key| present(info, key));
    if !calm {
        return None;
    }
    events.iter().find(|e| is_message(e, m)).and_then(|e| e.get("client_id")).and_then(Value::as_str).map(str::to_owned)
}

/// Whether the session's `state` says message `client` was answered and nothing else is on its way:
/// its delivery over (`completed`, `cancelled`), every other message's too, and kiln's queue empty. A
/// delivery its viewers are sent only a reference to says nothing.
fn answered_in(state: &Value, client: &str) -> bool {
    let over = |d: &Value| matches!(d.as_str(), Some("completed" | "cancelled"));
    let delivery = state.get("delivery").and_then(Value::as_object);
    delivery.is_some_and(|d| d.get(client).is_some_and(over) && d.values().all(over)) && state.get("queue").and_then(Value::as_array).is_none_or(Vec::is_empty)
}

/// The client id kiln keeps message `m` (a client id, or words its message carries) under: its
/// timeline's, where it shows there; else `m`.
fn client_of(events: &[Value], m: &str) -> String {
    events.iter().find(|e| is_message(e, m)).and_then(|e| e.get("client_id")).and_then(Value::as_str).unwrap_or(m).to_owned()
}

/// Whether the session's `state` has message `client` in kiln's queue: its delivery `queued`, or it
/// waits in the queue.
fn in_queue(state: &Value, client: &str) -> bool {
    state.get("delivery").and_then(|d| d.get(client)).and_then(Value::as_str) == Some("queued") || state.get("queue").and_then(Value::as_array).into_iter().flatten().any(|q| q.get("id").and_then(Value::as_str) == Some(client))
}

/// What holds message `m` (a client id, or words its message carries) in kiln's custody: a cause kiln
/// states that applies to it, said. Admission's own wait on it with its cause, as the session's feed
/// says it ([`Admission`]); or, where the session's `state` has it in kiln's queue ([`in_queue`]), the
/// session's room wait, pause or freeze, each holding all its queue, as kiln's word on it (`info`) says.
/// Never the admission capacity kiln's word carries, which names no message and states no cause. None:
/// no cause kiln states holds it.
pub fn holds(info: &Value, events: &[Value], state: Option<&Value>, admission: &Admission, m: &str) -> Option<String> {
    let client = client_of(events, m);
    if let Some(cause) = admission.waits(&client) {
        return Some(format!("{client} held by kiln's admission: {cause}"));
    }
    if !in_queue(state?, &client) {
        return None;
    }
    let why: Vec<String> = ["room_wait", "pause", "frozen"].iter().filter_map(|key| info.get(*key).filter(|v| !v.is_null()).map(|v| format!("{key} {v}"))).collect();
    (!why.is_empty()).then(|| format!("{client} in kiln's queue; {}", why.join(", ")))
}

/// What of a session moves as its harness and kiln work on it: kiln's word on it ([`MOVES`]) and its
/// timeline.
fn moving(info: &Value, events: &[Value]) -> Value {
    let word: Vec<Value> = MOVES.iter().map(|key| info.get(*key).cloned().unwrap_or(Value::Null)).collect();
    json!([word, events])
}

/// `text` on one line: its first [`TOLD`] characters, `…` where it's cut.
fn head(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(TOLD) {
        Some((cut, _)) => format!("{}…", flat.get(..cut).unwrap_or_default()),
        None => flat,
    }
}

/// A timeline event on one line: its order, kind, message id (a person's words), tool and status (a
/// call), level (a notice), and the first [`TOLD`] characters of what it says.
fn event_line(e: &Value) -> String {
    let field = |key: &str| e.get(key).and_then(Value::as_str);
    let mut line = format!("#{} {}", e.get("seq").and_then(Value::as_u64).unwrap_or_default(), field("kind").unwrap_or("?"));
    for key in ["client_id", "name", "status", "level"] {
        if let Some(v) = field(key) {
            let _ = write!(line, " {v}");
        }
    }
    let _ = write!(line, ": {}", head(field("text").or_else(|| field("output")).or_else(|| field("title")).unwrap_or_default()));
    line
}

/// kiln's word on a session, for a wait that ended without its answer: where it stands
/// ([`TOLD_FIELDS`]); each of its messages' delivery and its queue, from its state as its viewers get
/// it (`state`, or why it couldn't be read); and its timeline, an event a line ([`event_line`]).
pub fn told(info: &Value, state: &anyhow::Result<Value>, events: &[Value]) -> String {
    let mut out = String::new();
    let word: serde_json::Map<String, Value> = TOLD_FIELDS.iter().filter_map(|key| info.get(*key).filter(|v| !v.is_null()).map(|v| ((*key).to_owned(), v.clone()))).collect();
    let _ = writeln!(out, "kiln's word on it: {}", Value::Object(word));
    match state {
        Ok(s) => {
            let _ = writeln!(out, "its messages' delivery: {}", s.get("delivery").unwrap_or(&Value::Null));
            let queue: Vec<String> = s.get("queue").and_then(Value::as_array).into_iter().flatten().map(|q| format!("{} {}", q.get("id").and_then(Value::as_str).unwrap_or("?"), head(q.get("text").and_then(Value::as_str).unwrap_or_default()))).collect();
            let _ = writeln!(out, "its queue: {queue:?}");
            if let Some(deferred) = s.pointer("/view/deferred").filter(|d| d.as_object().is_some_and(|m| !m.is_empty())) {
                let _ = writeln!(out, "of its state, sent as references only: {deferred}");
            }
        }
        Err(e) => {
            let _ = writeln!(out, "its state (each message's delivery, its queue) couldn't be read: {e:#}");
        }
    }
    let _ = writeln!(out, "its timeline, {} events:", events.len());
    for e in events {
        let _ = writeln!(out, "  {}", event_line(e));
    }
    out
}

/// The session's state as kiln sends its viewers (each message's delivery, its queue): the state frame
/// of what its socket sends on attaching.
fn state_of(f: &Fixture, id: &str) -> anyhow::Result<Value> {
    let (frames, _) = common::agents::attach(f.port, &f.admin, id, "")?;
    frames.into_iter().find(|frame| frame.get("t").and_then(Value::as_str) == Some("state")).and_then(|frame| frame.get("state").cloned()).ok_or_else(|| anyhow::anyhow!("its viewers' socket sent no state"))
}

/// A wait on session `id` refused: typed, with kiln's word on it then and its runner's log.
fn refused(f: &Fixture, id: &str, what: &str, stall: Stall, info: &Value, events: &[Value]) -> anyhow::Error {
    let evidence = format!("{}its runner's log, its last lines:\n{}", told(info, &state_of(f, id), events), f.runner_log());
    anyhow::Error::new(Refused { what: what.to_owned(), stall, evidence })
}

impl Pace {
    /// The slowest answer so far.
    pub fn slowest(&self) -> Option<Duration> {
        self.slowest
    }

    /// The longest a wait on `asked` may go without progress: none before the drive's first answer,
    /// nor for a turn the drive asked to be long ([`Asked::Steered`]), which follow the harness's
    /// liveness and progress alone.
    pub fn window(&self, asked: Asked<'_>) -> Option<Duration> {
        if matches!(asked, Asked::Steered(_)) { None } else { self.slowest }
    }

    /// An answer that took `took`.
    pub fn answered(&mut self, took: Duration) {
        self.slowest = Some(self.slowest.map_or(took, |s| s.max(took)));
    }

    /// Waits on session `id` until `done` holds of kiln's word on it and its timeline, following its
    /// harness ([`Watch`]): the time its answer took is learned; refused, typed ([`Refused`]), where
    /// kiln's word says no answer can come, or once a whole window passes without progress where the
    /// drive has learned one ([`Pace::window`]). It wakes on the session's feed, whose frames it hears
    /// ([`Watch::heard`]), kept for the session's next wait.
    pub fn until(&mut self, f: &Fixture, id: &str, asked: Asked<'_>, what: &str, done: impl Fn(&Value, &[Value]) -> bool) -> anyhow::Result<(Value, Vec<Value>)> {
        let began = Instant::now();
        let mut watch = Watch::new(asked, self.window(asked));
        let mut wake = self.feeds.remove(id).unwrap_or_else(|| Wake::session(f, id));
        let mut said_held = String::new();
        loop {
            let looked = Instant::now();
            let (i, e) = (info(f, id)?, events(f, id)?);
            let held = done(&i, &e);
            let at = began.elapsed();
            // Its state read only where kiln's word says it may have answered, or its window runs out;
            // unread, it says nothing.
            let state = if !held && watch.wants_state(&i, &e, at) { state_of(f, id).ok() } else { None };
            let verdict = watch.look(&i, &e, state.as_ref(), held, at);
            if let Verdict::Held(why) = &verdict
                && *why != said_held
            {
                // Said as held, once a hold: the wait goes on.
                eprintln!("{what}: held by kiln, the wait going on: {why}");
                said_held.clone_from(why);
            }
            match verdict {
                Verdict::Done => {
                    self.answered(began.elapsed());
                    self.feeds.insert(id.to_owned(), wake);
                    return Ok((i, e));
                }
                Verdict::Wait | Verdict::Held(_) => {
                    for frame in wake.next(watch.left(began.elapsed()), looked.elapsed()) {
                        watch.heard(&frame);
                    }
                }
                Verdict::Refuse(stall) => {
                    self.feeds.insert(id.to_owned(), wake);
                    return Err(refused(f, id, what, stall, &i, &e));
                }
            }
        }
    }

    /// Waits until kiln lists session `id` no more (it was ended), as [`Pace::until`] waits: on kiln's
    /// word on it moving while it's listed, woken by kiln's events socket, which says each session's end.
    pub fn unlisted(&mut self, f: &Fixture, id: &str, what: &str) -> anyhow::Result<()> {
        let began = Instant::now();
        let mut watch = Watch::new(Asked::Kiln, self.window(Asked::Kiln));
        let mut wake = Wake::events(f);
        loop {
            let looked = Instant::now();
            let (code, body) = common::http(f.port, "GET", &format!("/api/harness/sessions/{id}"), &f.admin, None)?;
            if code == 404 {
                self.answered(began.elapsed());
                return Ok(());
            }
            anyhow::ensure!(code == 200, "{what}: GET its session: {code} {body}");
            let i: Value = serde_json::from_str(&body)?;
            match watch.look(&i, &[], None, false, began.elapsed()) {
                Verdict::Refuse(stall) => return Err(refused(f, id, what, stall, &i, &[])),
                Verdict::Done | Verdict::Wait | Verdict::Held(_) => {
                    wake.next(watch.left(began.elapsed()), looked.elapsed());
                }
            }
        }
    }

    /// Ends session `id` (the API's DELETE), and waits until kiln lists it no more ([`Pace::unlisted`]);
    /// its feed goes with it.
    pub fn stop(&mut self, f: &Fixture, id: &str) -> anyhow::Result<()> {
        // Longer than the server waits for its runner's answer (`hub::COMMAND_WITHIN`).
        let (code, body) = common::http_within(f.port, "DELETE", &format!("/api/harness/sessions/{id}"), &f.admin, None, Duration::from_secs(75))?;
        anyhow::ensure!(code == 204, "ending {id}: {code} {body}");
        self.feeds.remove(id);
        self.unlisted(f, id, &format!("ending {id}"))
    }
}

// ------------------------------------------------------------------------------------------------
// The drive: start, steer, interrupt, stop and resume, as a person does through kiln's API
// ------------------------------------------------------------------------------------------------

/// The stand-in's steered turn's gap between its lines, its own pace (`POLL * 8` it was, while the
/// drive looked every `POLL`): long enough for the drive to see the turn under way and steer a message
/// in, each a round trip through kiln, before its next line. No wait reads it: the steered turn's wait
/// follows the harness alone (`Asked::Steered`). Measured for it from a round trip, owed (the note).
const STEERED_GAP: Duration = Duration::from_secs(2);

/// The stand-in's long turn, steered into: its lines [`STEERED_GAP`] apart, so a message steered in as
/// the drive sees the turn begin lands while it still runs.
pub fn stub_steered_turn() -> String {
    format!("SLOW={} keep going", STEERED_GAP.as_millis())
}

/// The stand-in's turn to stop: it holds once it said what it heard, and only a stop ends it, so a
/// stop that isn't taken shows as the turn going on (no progress, or a line of it after the stop).
pub fn stub_stopped_turn() -> String {
    "HOLD go on until stopped".to_owned()
}

/// What a drive asks of one harness.
pub struct Ask<'a> {
    pub kind: &'a str,
    /// `{"kind": "microvm", "image": …}`; none: a process on its runner (the light run on the host).
    pub isolation: Option<Value>,
    /// The project, at its host path (canonical: the path the harness reports).
    pub project: &'a str,
    /// The first turn's words: ROOT's real task, or the stand-in's (`MCP_PING`, `SPAWN=<kind>`).
    pub prompt: String,
    /// The words the resumed session's turn takes, which end with [`Ask::marker`] too.
    pub resumed: String,
    /// A turn long enough to steer into ([`stub_steered_turn`], or ROOT's).
    pub steered: String,
    /// A turn long enough to stop ([`stub_stopped_turn`], or ROOT's).
    pub stopped: String,
    /// What the harness says once a turn ran to its end ([`STUB_DONE`], or ROOT's).
    pub marker: &'a str,
    /// Whether kiln gives this harness its MCP server where it runs: the caller's word (step 34's
    /// acceptance asks it of Claude Code and Codex in a microVM; step 35's gaps say it per harness).
    pub mcp: bool,
    /// Whether its first turn starts an agent from where it runs (through kiln's MCP server).
    pub child: bool,
    /// The file its first turn writes in its project, its name its words ([`check_written`]).
    pub probe: String,
}

impl Ask<'_> {
    /// Whether the stand-in harness is driven: it says what each step did, so each is checked.
    pub fn stub(&self) -> bool {
        self.marker == STUB_DONE
    }

    fn spec(&self, prompt: &str, resume: Option<&str>) -> Value {
        let mut spec = json!({"kind": self.kind, "cwd": self.project, "prompt": prompt});
        if let Some(o) = spec.as_object_mut() {
            if let Some(i) = &self.isolation {
                o.insert("isolation".into(), i.clone());
            }
            if let Some(r) = resume {
                o.insert("resume".into(), json!(r));
            }
        }
        spec
    }
}

/// What a drive saw, for the checks of where it ran.
pub struct Drove {
    /// The first session, and kiln's word on it after its first turn.
    pub id: String,
    pub info: Value,
    /// What its harness said in its first turn.
    pub first: Vec<String>,
    pub events: Vec<Value>,
    /// What each end of each link said it negotiated (`VM_E2E_LINKS`), read while it ran.
    pub links: Vec<LinkSuites>,
    /// The agent it started from where it ran, as kiln listed it, and the audit trail's rows on it.
    pub child: Option<Value>,
    pub audit: Vec<Value>,
    /// kiln's word on the first session just before it was stopped: where it ran and each start
    /// (`runs`, `starts`).
    pub last: Value,
    /// The session that took its conversation up again once it was ended, and kiln's word on it
    /// after its turn.
    pub resumed: String,
    pub resumed_info: Value,
    /// The host's harness processes under the runner, looked at between the steps.
    pub inventory: Inventory,
}

fn words(f: &Fixture, id: &str, cmd: Value) -> anyhow::Result<()> {
    command(f, id, &cmd)
}

/// A message to session `id` (`mode`: queue, steer).
fn send(f: &Fixture, id: &str, client: &str, text: &str, mode: &str) -> anyhow::Result<()> {
    words(f, id, json!({"type": "send", "id": client, "text": text, "mode": mode}))
}

/// Whether the turn of message `client` is under way: kiln delivered it and its harness works on it.
fn under_way(i: &Value, e: &[Value], client: &str) -> bool {
    delivered_at(e, client).is_some() && (status(i) == "running" || !said_after(e, client).is_empty())
}

/// A word of this drive's own, to tell its messages from any other's.
fn nonce() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    format!("{:x}{nanos:x}", std::process::id())
}

/// Drives `ask` on the fixture's runner, through kiln's API, as a person does: starts it with its
/// task, steers a message into a running turn, stops a turn, reads every link's suite, finds the
/// agent it started from where it runs, ends it, and takes its conversation up again in a new
/// session, which it ends too. Each step's own result is checked here; where it ran is the caller's
/// to check, from what this returns. `roots`: the pids whose process trees are looked at between the
/// steps ([`host_roots`]; none: not looked at).
pub fn drive(f: &Fixture, ask: &Ask<'_>, roots: &[u32], programs: &[&str]) -> anyhow::Result<Drove> {
    let kind = ask.kind;
    let nonce = nonce();
    let mut inventory = Inventory::new(programs);
    let look = |inv: &mut Inventory| if roots.is_empty() { Ok(()) } else { inv.look(roots) };
    // Each wait follows the harness, bounded by its slowest answer in this drive once it gave one.
    let mut pace = Pace::default();

    // Start: its first turn runs to its end.
    let id = open(f, &ask.spec(&ask.prompt, None))?;
    let (first_info, events) = pace.until(f, &id, Asked::Opening, &format!("{kind}'s first turn"), |i, e| status(i) == "idle" && said_marker(&said(e), ask.marker))?;
    look(&mut inventory)?;
    let first = said(&events);

    // Steer: a message into a turn under way (where the harness takes none mid-turn, kiln sends it
    // next), answered.
    let (long, steer) = (format!("long-{nonce}"), format!("steer-{nonce}"));
    send(f, &id, &long, &ask.steered, "queue")?;
    pace.until(f, &id, Asked::Message(&long), &format!("{kind}'s long turn under way"), |i, e| under_way(i, e, &long))?;
    send(f, &id, &steer, &format!("STEER {nonce}"), "steer")?;
    pace.until(f, &id, Asked::Steered(&steer), &format!("{kind} answering what was steered in"), |i, e| {
        status(i) == "idle" && delivered_at(e, &steer).is_some() && (!ask.stub() || said(e).iter().any(|l| (l.starts_with("STEERED ") || l.starts_with("HEARD ")) && l.contains(&nonce)))
    })?;
    look(&mut inventory)?;

    // Interrupt: a turn under way stops at once. The stand-in's holds once it said what it heard, and
    // only a stop ends it: a line of it after the stop was taken says it didn't stop.
    let stop_me = format!("stop-{nonce}");
    send(f, &id, &stop_me, &ask.stopped, "queue")?;
    pace.until(f, &id, Asked::Message(&stop_me), &format!("{kind}'s turn to stop under way"), |i, e| under_way(i, e, &stop_me) && (!ask.stub() || said_after(e, &stop_me).iter().any(|l| l.starts_with("HEARD "))))?;
    look(&mut inventory)?;
    words(f, &id, json!({"type": "interrupt"}))?;
    let held = if ask.stub() { Some(said(&self::events(f, &id)?).len()) } else { None };
    let (_, e) = pace.until(f, &id, Asked::Stop { said: held }, &format!("{kind} stopping its turn"), |i, _| status(i) == "idle")?;
    if ask.stub() {
        anyhow::ensure!(!said_marker(&said_after(&e, &stop_me), ask.marker), "{kind}: the stopped turn ran to its end");
    }

    // Every link's suite, from both ends, while the session runs.
    let mut links = Vec::new();
    if let Ok(text) = std::env::var("VM_E2E_LINKS") {
        for l in link_specs(&text).map_err(|e| anyhow::anyhow!(e))? {
            let named = |s: &str| s.replace("{session}", &id).replace("{machine}", &f.machine);
            let read = |e: &End| -> anyhow::Result<Value> {
                let body = f.get(&named(&e.url))?;
                let pick = e.pick.as_ref().map(|(field, value)| (field.clone(), named(value)));
                Ok(picked(&body, pick.as_ref()).cloned().unwrap_or(Value::Null))
            };
            let end = |e: &End| -> anyhow::Result<Option<String>> { Ok(suite_at(&read(e)?, &e.pointer)) };
            let lanes = l.lanes.as_ref().map(|e| -> anyhow::Result<Option<Vec<String>>> { Ok(lanes_at(&read(e)?, &e.pointer)) }).transpose()?.flatten();
            links.push(LinkSuites { client: end(&l.client)?, server: end(&l.server)?, lanes, name: l.name });
        }
    }

    // The agent it started from where it runs, and the audit trail's word on its start; then it's
    // ended. kiln lists it as it answers its start, inside the first turn: kiln's work, waited on in
    // the drive's window.
    let (mut child, mut audit) = (None, Vec::new());
    if ask.child {
        let child_of = || f.get("/api/harness/sessions").ok().and_then(|all| all.as_array().into_iter().flatten().find(|s| s.get("parent").and_then(Value::as_str) == Some(id.as_str())).cloned());
        pace.until(f, &id, Asked::Kiln, &format!("{kind} started no agent from where it runs that kiln lists; it said {first:?}"), |_, _| child_of().is_some())?;
        let found = child_of().ok_or_else(|| anyhow::anyhow!("{kind} started no agent from where it runs; it said {first:?}"))?;
        audit = f.get(&format!("/api/audit?q={}&limit=1000", id.get(..8).unwrap_or(&id)))?.as_array().cloned().unwrap_or_default();
        if let Some(c) = found.get("id").and_then(Value::as_str) {
            pace.stop(f, c)?;
        }
        child = Some(found);
    }

    // Stop: ended for good.
    let last = info(f, &id)?;
    let hs = last.get("harness_session").and_then(Value::as_str).map(str::to_owned);
    pace.stop(f, &id)?;

    // Resume: a new session takes its conversation up again, its harness's own session.
    let hs = hs.ok_or_else(|| anyhow::anyhow!("{kind}: kiln knows no harness session of {id} to take up again"))?;
    let resumed = open(f, &ask.spec(&ask.resumed, Some(&hs)))?;
    let (resumed_info, _) = pace.until(f, &resumed, Asked::Opening, &format!("{kind} taking its conversation up again"), |i, e| status(i) == "idle" && said_marker(&said(e), ask.marker))?;
    look(&mut inventory)?;
    let took_up = resumed_info.get("harness_session").unwrap_or(&Value::Null);
    anyhow::ensure!(took_up.as_str() == Some(hs.as_str()), "{kind}: the resumed session's harness session is {took_up}, not {hs}");
    pace.stop(f, &resumed)?;
    Ok(Drove { id, info: first_info, first, events, links, child, audit, last, resumed, resumed_info, inventory })
}

// ------------------------------------------------------------------------------------------------
// One harness's run: in a microVM of its own, or on a Unix host as a process behind the stand-in
// ------------------------------------------------------------------------------------------------

/// The stand-in harness (`tests/fixtures/vm-stub/harness/stub`), a Python script run by its name.
pub fn stub_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/vm-stub/harness/stub")
}

/// kiln's own name for kind's program, which its runner starts (`Kind::program`).
pub fn program(kind: &str) -> anyhow::Result<&'static str> {
    kiln::harness::kinds::Kind::get(kind).map(|k| k.program).ok_or_else(|| anyhow::anyhow!("kiln has no harness {kind}"))
}

/// The stand-in's first turn: the file it writes in its project, and where kiln gives it its MCP
/// server a round trip to it and (`child`) an agent of its own kind started through it from where
/// it runs.
pub fn stub_prompt(kind: &str, mcp: bool, child: bool, probe: &str) -> String {
    let mut words = vec![format!("WRITE={probe}")];
    if mcp {
        words.push("MCP_PING".to_owned());
        if child {
            words.push(format!("SPAWN={kind}"));
        }
    }
    words.push("do the task".to_owned());
    words.join(" ")
}

/// ROOT's words for a real task (`var`; `{kind}` the harness, `{file}` the file it writes in its
/// project), else the stand-in's.
pub fn task(var: &str, kind: &str, probe: &str, stub: String) -> String {
    std::env::var(var).map(|p| p.replace("{kind}", kind).replace("{file}", probe)).unwrap_or(stub)
}

/// The words of a real task, each of a real harness's turns: ROOT's alone.
pub const TASK_WORDS: [&str; 5] = ["VM_E2E_PROMPT", "VM_E2E_RESUMED_PROMPT", "VM_E2E_STEERED_PROMPT", "VM_E2E_STOPPED_PROMPT", "VM_E2E_DONE_MARKER"];

/// Why a drive of `kind` from `image` doesn't run, or none. kiln's own image for a harness
/// (`.kiln/images/harness-<kind>`) runs the vendor's CLI, which takes ROOT's task alone: never the
/// stand-in's words sent to a model, so each of [`TASK_WORDS`] not `given` is named. An image ROOT
/// names (`VM_E2E_IMAGE_<KIND>`, `agentfile:<id>`: the stand-in's) takes the stand-in's where ROOT
/// gives none.
pub fn untasked(kind: &str, image: &str, given: impl Fn(&str) -> bool) -> Option<String> {
    if !image.starts_with("harness-") {
        return None;
    }
    let missing: Vec<&str> = TASK_WORDS.iter().copied().filter(|v| !given(v)).collect();
    (!missing.is_empty()).then(|| format!("SKIP: {kind} from {image} is the vendor's own CLI, which takes ROOT's task alone, and {} isn't given", missing.join(", ")))
}

/// The project: ROOT's (`VM_E2E_PROJECT`, a real project's folder on this host), else one made under
/// `dir`. Canonical, as a harness reports its folder (macOS's /var is /private/var), and without the
/// verbatim prefix canonicalizing adds on Windows, which kiln's paths don't carry.
pub fn project(dir: &Path) -> anyhow::Result<String> {
    let p = match std::env::var_os("VM_E2E_PROJECT") {
        Some(p) => PathBuf::from(p),
        None => {
            let p = dir.join("project");
            std::fs::create_dir_all(&p)?;
            std::fs::write(p.join("README.md"), "the project\n")?;
            p
        }
    };
    let full = std::fs::canonicalize(&p)?.to_string_lossy().into_owned();
    Ok(full.strip_prefix(r"\\?\").map_or_else(|| full.clone(), str::to_owned))
}

/// The image kind's microVM is made from: `VM_E2E_IMAGE_<KIND>` (ROOT's), else kiln's Agentfile for
/// it.
pub fn builtin_image(kind: &str) -> String {
    std::env::var(format!("VM_E2E_IMAGE_{}", kind.to_ascii_uppercase())).unwrap_or_else(|_| format!("harness-{kind}"))
}

/// `VM_E2E_IMAGE_<KIND>`'s word for the stand-in as kind's image: the drive keeps its Agentfile in
/// kiln for the run ([`keep_stub`]) and boots `agentfile:<id>`, so no vendor's CLI runs anywhere.
pub const STUB_IMAGE: &str = "stub";

/// The stand-in's harness directory in kiln's repository, and where its image has it (its
/// `harness.json` runs it there).
pub const STUB_HARNESS: (&str, &str) = ("crates/kiln/tests/fixtures/vm-stub/harness", "/opt/vm-stub");

/// The stand-in's home in its microVM, every kind's: its harness.json's `run.env` `HOME`.
pub const STUB_HOME: &str = "/home/stub";

/// The stand-in's Agentfile for harness `kind` (its program `program`), its text alone, as kiln
/// builds a person's `agentfile:<id>` (kiln plan step 17: no build context, no build arguments): kiln's
/// own images' base (`base`, their `ARG BASE=` line); the harness named as kind's, which kiln starts
/// by that name, from `repository` at `commit` (fetched by Shards' own git client where the runner
/// is) to [`STUB_HARNESS`]'s path, speaking kind's interface; its home as kiln's images mount theirs:
/// its harness.json's `run.env` sets `HOME` to [`STUB_HOME`], one for every kind, and that is its
/// harness's volume, as kiln refuses a person's Agentfile whose harness keeps its configuration in no
/// volume (`harness::isolated::Refusal::NoConfigVolume`; its project is plugged in at its host path, as
/// every harness's is: Shards D235); and no network at all, as it calls no model and reaches kiln on
/// the agent-control link alone (default deny; D254).
pub fn stub_agentfile(kind: &str, program: &str, commit: &str, base: &str, repository: &str) -> String {
    let (dir, at) = STUB_HARNESS;
    format!(
        "{base}\nFROM ${{BASE}}\nENV VM_STUB_PROTOCOL={program}\nHARNESS {kind} FROM {repository}.git#{commit}:{dir} TO {at}\nVOLUME --target-kind=harness {STUB_HOME} FOR {kind}\n"
    )
}

/// kiln's own images' base, as their `ARG BASE=` line names it (`.kiln/images/harness-claude`).
pub fn kiln_base() -> anyhow::Result<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.kiln/images/harness-claude/Agentfile");
    let text = std::fs::read_to_string(&path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    text.lines().find(|l| l.starts_with("ARG BASE=")).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("{} names no ARG BASE", path.display()))
}

/// kiln's repository, as its workspace declares it (`Cargo.toml`'s `[workspace.package]`).
pub fn kiln_repository() -> anyhow::Result<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml");
    let manifest: toml::Table = toml::from_str(&std::fs::read_to_string(&path)?)?;
    let repository = manifest.get("workspace").and_then(|w| w.get("package")).and_then(|p| p.get("repository")).and_then(toml::Value::as_str);
    repository.map(str::to_owned).ok_or_else(|| anyhow::anyhow!("{} declares no repository", path.display()))
}

/// The commit this checkout is at, which the stand-in's image is built from: Shards fetches it from
/// kiln's repository, so it must be there.
pub fn this_commit() -> anyhow::Result<String> {
    let out = std::process::Command::new("git").arg("-C").arg(env!("CARGO_MANIFEST_DIR")).args(["rev-parse", "HEAD"]).output()?;
    anyhow::ensure!(out.status.success(), "git rev-parse HEAD: {}", String::from_utf8_lossy(&out.stderr));
    Ok(String::from_utf8(out.stdout)?.trim().to_owned())
}

/// An Agentfile the drive keeps in kiln for a run, removed as it drops.
pub struct Kept<'a> {
    f: &'a Fixture,
    pub id: String,
}

impl Drop for Kept<'_> {
    fn drop(&mut self) {
        let _ = common::http(self.f.port, "DELETE", &format!("/api/agentfiles/{}", self.id), &self.f.admin, None);
    }
}

/// The stand-in kept in kiln as kind's image ([`stub_agentfile`], at [`this_commit`]).
pub fn keep_stub<'a>(f: &'a Fixture, kind: &str) -> anyhow::Result<Kept<'a>> {
    let text = stub_agentfile(kind, program(kind)?, &this_commit()?, &kiln_base()?, &kiln_repository()?);
    let ask = json!({"name": format!("vm-stub-{kind}-{}", nonce()), "text": text});
    let (code, body) = common::http(f.port, "POST", "/api/agentfiles", &f.admin, Some(&ask))?;
    anyhow::ensure!((200..300).contains(&code), "kiln kept no Agentfile for the stand-in as {kind}: {code} {body}");
    let saved: Value = serde_json::from_str(&body)?;
    let id = saved.pointer("/agentfile/id").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("kiln named no Agentfile for the stand-in as {kind}: {body}"))?;
    Ok(Kept { f, id: id.to_owned() })
}

/// The Agentfile `image` is made from: a person's as kiln keeps it (`agentfile:<id>`), else kiln's
/// own, as kiln carries it (`.kiln/images/<image>/Agentfile`, `agentfiles::builtin`).
pub fn agentfile_of(f: &Fixture, image: &str) -> anyhow::Result<String> {
    if let Some(id) = image.strip_prefix("agentfile:") {
        let kept = f.get(&format!("/api/agentfiles/{id}"))?;
        return kept.get("text").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("kiln keeps no text for {image}: {kept}"));
    }
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.kiln/images").join(image).join("Agentfile");
    std::fs::read_to_string(&path).map_err(|e| anyhow::anyhow!("{image}'s Agentfile ({}): {e}", path.display()))
}

/// The ports a microVM's grants must not reach kiln at: the server this run drives, and kiln's
/// default, which kiln's own images name.
fn kiln_ports(f: &Fixture) -> Vec<u16> {
    let mut ports = vec![f.port];
    ports.extend(kiln_default_port().filter(|p| *p != f.port));
    ports
}

/// A file name of this drive's own, for its harness to write in its project.
fn probe_name() -> String {
    format!("vm-e2e-{}.txt", nonce())
}

/// [`check_written`], then the file removed, so a real project keeps nothing of the run.
fn written_and_removed(project: &str, probe: &str) -> Result<(), String> {
    let at = Path::new(project);
    let found = check_written(at, probe);
    let _ = std::fs::remove_file(at.join(probe));
    found
}

/// Drives `kind` in a microVM made from `image`, in `project`, and checks it ran there and nowhere
/// else: kiln holds it isolated; the project at its host path, inside and out; kiln's MCP from
/// inside where kiln gives it (`mcp`), on the agent-control link's lane alone ([`MCP_LANE`]), and an
/// agent started from there (`child`); no harness in the session's process trees on the host at any
/// look; a Linux kernel of its own; every link's suite at both ends. A vendor's own CLI runs only
/// with ROOT's task ([`untasked`]): it skips without it, and `VM_E2E=1` makes that a failure.
pub fn in_microvm(f: &Fixture, kind: &str, image: &str, project: &str, mcp: bool, child: bool) -> anyhow::Result<()> {
    if let Some(why) = untasked(kind, image, |v| std::env::var_os(v).is_some()) {
        anyhow::ensure!(!isolation_required(), "{why} (VM_E2E=1 requires a real task)");
        eprintln!("{why}");
        return Ok(());
    }
    let kept = if image == STUB_IMAGE { Some(keep_stub(f, kind)?) } else { None };
    let image = kept.as_ref().map_or_else(|| image.to_owned(), |k| format!("agentfile:{}", k.id));
    let image = image.as_str();
    let to_kiln = grants_to_kiln(&agentfile_of(f, image)?, &kiln_ports(f)).map_err(|e| anyhow::anyhow!("{image}: {e}"))?;
    anyhow::ensure!(to_kiln.is_empty(), "{kind} from {image}: its image grants a way to kiln past the agent-control link's {}<member> lane (Shards D254): {to_kiln:?}", MCP_LANE.1);
    let marker = std::env::var("VM_E2E_DONE_MARKER").unwrap_or_else(|_| STUB_DONE.to_owned());
    let probe = probe_name();
    let prompt = task("VM_E2E_PROMPT", kind, &probe, stub_prompt(kind, mcp, child, &probe));
    let ask = Ask {
        kind,
        isolation: Some(json!({"kind": "microvm", "image": image})),
        project,
        resumed: task("VM_E2E_RESUMED_PROMPT", kind, &probe, format!("{prompt} again")),
        prompt,
        steered: task("VM_E2E_STEERED_PROMPT", kind, &probe, stub_steered_turn()),
        stopped: task("VM_E2E_STOPPED_PROMPT", kind, &probe, stub_stopped_turn()),
        marker: &marker,
        mcp,
        child: mcp && child,
        probe: probe.clone(),
    };
    let programs = [program(kind)?, "stub"];
    let d = drive(f, &ask, &host_roots(f.runner_pid)?, &programs)?;
    let what = format!("{kind} from {image}");
    let fail = |e: String| anyhow::anyhow!("{what}: {e}");
    anyhow::ensure!(d.info.pointer("/isolation/kind").and_then(Value::as_str) == Some("microvm"), "{what}: kiln holds it as {}", d.info.get("isolation").unwrap_or(&Value::Null));
    check_project_path(d.info.get("cwd").and_then(Value::as_str), cwd_said(&d.first).as_deref(), project, std::env::consts::OS).map_err(fail)?;
    written_and_removed(project, &probe).map_err(fail)?;
    if mcp {
        mcp_round_trip(&d.events, &d.first).map_err(fail)?;
        check_mcp_lane(&d.links).map_err(fail)?;
        anyhow::ensure!(mcp_calls_over_lane(&f.runner_log_whole()?, &d.id) > 0, "{what}: its runner's log says no call of kiln's MCP over the session's lane ({MCP_LANE_SAID:?}, session={})", d.id);
    }
    if let Some(c) = &d.child {
        check_child(c, &d.id, &vm_runners(&f.get("/api/reach/runners")?)).map_err(fail)?;
        check_child_start(&d.audit, c, &d.id).map_err(fail)?;
    }
    // Nothing of it ran outside its microVM: no harness in the session's trees at any look, and the
    // kernel it said it ran on is a guest's.
    anyhow::ensure!(d.inventory.looks > 0, "{what}: the host's processes were never looked at (VM_E2E_RUNNER_PID names ROOT's runner)");
    anyhow::ensure!(d.inventory.seen.is_empty(), "{what}: these ran on the host, outside its microVM: {:?}", d.inventory.seen);
    check_in_guest(where_said(&d.first).as_ref(), std::env::consts::OS, host_boot_id().as_deref()).map_err(fail)?;
    // kiln's own word: no process of the host held, and every start of its harness in a microVM.
    for (which, said) in [("its session", &d.last), ("its resumed session", &d.resumed_info)] {
        check_no_host_pid(said).and_then(|()| check_starts(said)).map_err(|e| fail(format!("{which}: {e}")))?;
    }
    check_all_links(&d.links).map_err(fail)?;
    Ok(())
}

/// kiln's server and a runner whose PATH, and whose login shell's (`Kind::search_path`), finds the
/// stand-in under each harness's name and no real harness: a real one is never started here.
#[cfg(unix)]
pub fn host_fixture(dir: &Path) -> anyhow::Result<Fixture> {
    use std::os::unix::fs::PermissionsExt as _;
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin)?;
    for k in kiln::harness::kinds::Kind::all() {
        std::os::unix::fs::symlink(stub_path(), bin.join(k.program))?;
    }
    let path = format!("{}:{}", bin.display(), common::agents::path_without_harnesses(dir)?);
    let shell = dir.join("login-shell");
    std::fs::write(&shell, format!("#!/bin/sh\nPATH='{path}'\nexport PATH\nwhile [ $# -gt 1 ]; do shift; done\nexec /bin/sh -c \"$1\"\n"))?;
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755))?;
    up(dir, "box", &[("PATH", &path), ("SHELL", &shell.to_string_lossy())])
}

/// Drives `kind` as a process on this host, the stand-in behind its name, through kiln, with the
/// drive [`in_microvm`] uses: every step passes, and every check of where it ran says "the host"
/// (the stand-in seen in the runner's tree at every look, the host's kernel, no link read), as it
/// must. `mcp`: whether kiln gives this harness its MCP server on this host.
#[cfg(unix)]
pub fn on_host(kind: &str, mcp: bool) -> anyhow::Result<()> {
    if std::process::Command::new("python3").arg("--version").output().is_err() {
        eprintln!("SKIP: the stand-in harness needs python3");
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let f = host_fixture(dir.path())?;
    let project = project(dir.path())?;
    let probe = probe_name();
    let prompt = stub_prompt(kind, mcp, mcp, &probe);
    let ask = Ask {
        kind,
        isolation: None,
        project: &project,
        resumed: format!("{prompt} again"),
        prompt,
        steered: stub_steered_turn(),
        stopped: stub_stopped_turn(),
        marker: STUB_DONE,
        mcp,
        child: mcp,
        probe: probe.clone(),
    };
    let programs = [program(kind)?, "stub"];
    let d = drive(&f, &ask, &host_roots(f.runner_pid)?, &programs)?;
    let fail = |e: String| anyhow::anyhow!("{kind}: {e}");
    check_project_path(d.info.get("cwd").and_then(Value::as_str), cwd_said(&d.first).as_deref(), &project, std::env::consts::OS).map_err(fail)?;
    written_and_removed(&project, &probe).map_err(fail)?;
    if mcp {
        mcp_round_trip(&d.events, &d.first).map_err(fail)?;
        let child = d.child.as_ref().ok_or_else(|| anyhow::anyhow!("{kind} started no agent"))?;
        check_child_start(&d.audit, child, &d.id).map_err(fail)?;
        // A process on its machine, as its parent is: the isolation check says so.
        anyhow::ensure!(check_child(child, &d.id, std::slice::from_ref(&f.machine)).is_err_and(|e| e.contains("isn't held in one")), "{kind}: {child}");
    } else {
        anyhow::ensure!(mcp_round_trip(&d.events, &d.first).is_err(), "{kind} reached an MCP server kiln never gave it");
    }
    // Where it ran: the host, which each check of a microVM says.
    anyhow::ensure!(!d.inventory.seen.is_empty() && d.inventory.looks >= 4, "{kind}: the harness ran on the host, but the inventory never saw it in {} looks", d.inventory.looks);
    let said_where = where_said(&d.first);
    anyhow::ensure!(check_in_guest(said_where.as_ref(), std::env::consts::OS, host_boot_id().as_deref()).is_err(), "{kind}: the kernel check took the host's for a guest's: {said_where:?}");
    anyhow::ensure!(check_all_links(&d.links).is_err(), "{kind}: links read where there are none");
    if mcp {
        anyhow::ensure!(mcp_calls_over_lane(&f.runner_log_whole()?, &d.id) == 0, "{kind}: the runner's log says a session on the host called kiln's MCP over an agent-control lane");
    }
    anyhow::ensure!(check_no_host_pid(&d.last).is_err() && check_starts(&d.last).is_err(), "{kind}: kiln says a session on the host ran in a microVM: {}", d.last.get("runs").unwrap_or(&Value::Null));
    Ok(())
}
