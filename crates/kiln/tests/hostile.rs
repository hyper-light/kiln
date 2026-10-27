//! Typed messages, step 10 (docs/wire.md, "What can't be read"): every wire kiln reads, as its
//! inventory names them (`kiln::wire::WIRES`), is sent five messages it can't read:
//! - an unknown one: a kind this build doesn't know;
//! - a malformed one: a kind it knows, that can't be read as one;
//! - one cut short;
//! - one with bytes left over;
//! - one too large.
//!
//! Each is held to what its sender is told, the counter (`kiln_wire_refused_total`), the audit row
//! naming where it came from, its page's record, and the next valid message on that wire being
//! applied. Every wire's cases are in [`rows`], matched without a catch-all arm, and
//! `every_wire_has_its_cases` fails for a wire of the inventory without all five, or without a test
//! that sends them.
//!
//! Who sends what, over kiln's real processes and net layer:
//! - what the server reads of a runner's (its frames, its channel's items, the JSON they carry, what
//!   it asks the relay): a runner of the test's own ([`Peer`]), its certificate from the server's CA,
//!   which sends what it's given byte for byte;
//! - what a runner reads of the server's: a test build's server, told to send a real runner what it
//!   never would (`POST /api/chaos/send`);
//! - what a runner reads inside a reach link: the test's runner, through the relay, to a real
//!   runner, over the link's end-to-end TLS;
//! - MCP: over HTTP, and to a real runner's own MCP server (`kiln reach mcp`);
//! - the UI's sockets: a socket of the test's own.
//!
//! A message too large, on a wire carried in kiln's frames, is a frame past its lane's bound (1 MiB):
//! its sender's transport refuses it before the wire, counted `refused` among what it let go below
//! the channel, which its runner's page shows; a receiver's transport skips one that comes, so no
//! reader of kiln's ever has it to audit. A command too large for its frame is answered so at once.
//! Where kiln bounds a message itself (a reach link's 4 MiB, MCP's 16 MiB), it's refused and noted
//! as any other.
//!
//! The UI's sockets (typed by step 6 of the plan) are each held to their typed answer, the count,
//! the audit row naming the person and the page, and to going on: the next valid message on the
//! socket is applied.
//!
//! `cargo test -p kiln --features chaos --test hostile`: each wire's cases print as they pass.

#![cfg(all(unix, feature = "chaos"))]

mod common;

use std::collections::BTreeMap;
use std::io::{BufRead as _, Read as _, Write as _};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use base64::Engine as _;
use common::agents::{self, Talk};
use common::{Options, Server, audited, http, kept, metric, record, refused};
use kiln::net::{Kind, NetEvent, NetHandle, Out, SERVER_PEER};
use kiln::proto::{self, AgentMsg, ServerMsg};
use kiln::wire::WIRES;
use kiln::wire::channel::{Frame, Refusal};
use kiln::wire::unread::{Code, Wire};
use serde_json::{Value, json};

/// A frame past every lane's bound (1 MiB for a control or terminal frame).
const LARGE: usize = (1 << 20) + 4096;

/// A line of what passed, written to stderr as it is: a run that passes shows it without
/// `--nocapture` (libtest keeps what `eprintln!` writes for the runs that fail).
fn say(line: &str) {
    let _ = std::io::stderr().write_all(format!("{line}\n").as_bytes());
}

// ------------------------------------------------------------------------------------- the cases

/// The five cases each wire is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Case {
    Unknown,
    Malformed,
    Short,
    LeftOver,
    TooLarge,
}

const CASES: [Case; 5] = [Case::Unknown, Case::Malformed, Case::Short, Case::LeftOver, Case::TooLarge];

impl Case {
    fn name(self) -> &'static str {
        match self {
            Case::Unknown => "unknown",
            Case::Malformed => "malformed",
            Case::Short => "cut short",
            Case::LeftOver => "bytes left over",
            Case::TooLarge => "too large",
        }
    }
}

/// Where a case's refusal shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Noted {
    /// Refused and noted on this wire with this code, from where it came: counted, audited naming
    /// it, kept for its page.
    As(Wire, Code),
    /// Never read: a frame past its lane's bound, which its sender's transport refused (counted
    /// `refused` among what it let go below the channel, on its runner's page). Nothing reached a
    /// reader to audit.
    Below,
}

/// A wire's case: what's sent, what its sender is told, and where it's noted.
struct Row {
    case: Case,
    sent: &'static str,
    told: &'static str,
    noted: Noted,
}

const fn row(case: Case, sent: &'static str, told: &'static str, noted: Noted) -> Row {
    Row { case, sent, told, noted }
}

/// The five cases of a UI socket's way up: frames it can't read, sent up.
const fn up(w: Wire, told: &'static str) -> [Row; 5] {
    [
        row(Case::Unknown, "a frame whose tag it doesn't know", told, Noted::As(w, Code::Unknown)),
        row(Case::Malformed, "a frame that isn't JSON", told, Noted::As(w, Code::Malformed)),
        row(Case::Short, "a frame cut short", told, Noted::As(w, Code::Malformed)),
        row(Case::LeftOver, "a frame with bytes after its JSON", told, Noted::As(w, Code::Malformed)),
        row(Case::TooLarge, "a frame past the socket's limit", "the socket closed, 1009", Noted::As(w, Code::Malformed)),
    ]
}

/// The five cases of a UI socket's way down: what a page couldn't read of what came down, said up
/// (`{"t": "unread", "of", "why"}`), so the server, which sent it, notes it.
const fn down(w: Wire) -> [Row; 5] {
    // The tunnel's client is a runner, known by its address; the rest are a person's pages.
    let told = if matches!(w, Wire::TunnelDown) { "nothing: noted from its address" } else { "nothing: noted from the person" };
    [
        row(Case::Unknown, "unread: a frame whose tag the page doesn't know", told, Noted::As(w, Code::Unknown)),
        row(Case::Malformed, "unread: a frame that isn't JSON", told, Noted::As(w, Code::Malformed)),
        row(Case::Short, "unread: a frame cut short", told, Noted::As(w, Code::Malformed)),
        row(Case::LeftOver, "unread: a frame with bytes after its JSON", told, Noted::As(w, Code::Malformed)),
        row(Case::TooLarge, "unread: a frame past what the page reads", told, Noted::As(w, Code::Malformed)),
    ]
}

/// Every wire's cases, matched without a catch-all arm: a wire added to the inventory doesn't
/// build until it has them.
fn rows(w: Wire) -> [Row; 5] {
    use Case::*;
    use Code::{Malformed as M, Unknown as U};
    match w {
        Wire::Runner => [
            row(Unknown, "a frame of a variant past AgentMsg's", "ServerMsg::Unread, unknown", Noted::As(w, U)),
            row(Malformed, "a Power frame mangled past its index", "ServerMsg::Unread, malformed", Noted::As(w, M)),
            row(Short, "a Power frame cut short", "ServerMsg::Unread, malformed: cut short", Noted::As(w, M)),
            row(LeftOver, "a Power frame with 3 bytes after it", "ServerMsg::Unread, malformed: 3 bytes left over", Noted::As(w, M)),
            row(TooLarge, "a Power frame past 1 MiB", "its own transport refuses it", Noted::Below),
        ],
        Wire::Server => [
            row(Unknown, "a frame of a variant past ServerMsg's", "the runner says so: AgentMsg::Unread, unknown", Noted::As(w, U)),
            row(Malformed, "a TermResize frame mangled past its index", "the runner says so: AgentMsg::Unread, malformed", Noted::As(w, M)),
            row(Short, "a Cancel frame cut short", "the runner says so: AgentMsg::Unread, malformed", Noted::As(w, M)),
            row(LeftOver, "a Cancel frame with 3 bytes after it", "the runner says so: AgentMsg::Unread, malformed", Noted::As(w, M)),
            row(TooLarge, "a frame past 1 MiB", "the server's transport refuses it", Noted::Below),
        ],
        Wire::RunnerItem => [
            row(Unknown, "an item of a variant past AgentMsg's", "its NACK, unsupported", Noted::As(w, U)),
            row(Malformed, "an item: a Power mangled past its index", "its NACK, malformed", Noted::As(w, M)),
            row(Short, "an item: a Power cut short", "its NACK, malformed", Noted::As(w, M)),
            row(LeftOver, "an item: a Power with 3 bytes after it", "its NACK, malformed", Noted::As(w, M)),
            row(TooLarge, "an item whose frame is past 1 MiB", "its own transport refuses it; its number goes to the next", Noted::Below),
        ],
        Wire::ServerItem => [
            row(Unknown, "a command of a variant past ServerMsg's", "the runner's NACK, unsupported: the command refused", Noted::As(w, U)),
            row(Malformed, "a command: a TermResize mangled past its index", "the runner's NACK, malformed", Noted::As(w, M)),
            row(Short, "a command: a TermResize cut short", "the runner's NACK, malformed", Noted::As(w, M)),
            row(LeftOver, "a command: a TermResize with 3 bytes after it", "the runner's NACK, malformed", Noted::As(w, M)),
            row(TooLarge, "a command past what a frame carries", "answered at once: too large to send", Noted::Below),
        ],
        Wire::RunnerPayload => [
            row(Unknown, "ReachAsk: a question of a kind the server doesn't know", "ServerMsg::Unread, unknown; its question answered why", Noted::As(w, U)),
            row(Malformed, "ReachAsk: a call whose name is a number", "ServerMsg::Unread, malformed; its question answered why", Noted::As(w, M)),
            row(Short, "ReachAsk: its JSON cut short", "ServerMsg::Unread, malformed; its question answered why", Noted::As(w, M)),
            row(LeftOver, "ReachAsk: its JSON with more after it", "ServerMsg::Unread, malformed; its question answered why", Noted::As(w, M)),
            row(TooLarge, "ReachAsk: its frame past 1 MiB", "its own transport refuses it", Noted::Below),
        ],
        Wire::ServerPayload => [
            row(Unknown, "HarnessCmd: a command of a type the runner doesn't know", "the runner's NACK, unsupported; on the session's page too", Noted::As(w, U)),
            row(Malformed, "HarnessCmd: set_model, its model a number", "the runner's NACK, malformed; on the session's page too", Noted::As(w, M)),
            row(Short, "HarnessCmd: its JSON cut short", "the runner's NACK, malformed; on the session's page too", Noted::As(w, M)),
            row(LeftOver, "HarnessCmd: its JSON with more after it", "the runner's NACK, malformed; on the session's page too", Noted::As(w, M)),
            row(TooLarge, "HarnessCmd: past what a frame carries", "answered at once: too large to send", Noted::Below),
        ],
        // The relay reads a frame whole first: one it can't read is a runner's frame it can't read.
        Wire::Relay => [
            row(Unknown, "on reach's lane, a frame of a variant past AgentMsg's", "ServerMsg::Unread, unknown, as the runner's frame it is", Noted::As(Wire::Runner, U)),
            row(Malformed, "Reach on another pair's link", "ServerMsg::Unread, malformed: not its link, naming its ends; never forwarded", Noted::As(w, M)),
            row(Short, "a Reach frame cut short", "ServerMsg::Unread, malformed, as the runner's frame it is", Noted::As(Wire::Runner, M)),
            row(LeftOver, "a Reach frame with 3 bytes after it", "ServerMsg::Unread, malformed, as the runner's frame it is", Noted::As(Wire::Runner, M)),
            row(TooLarge, "a Reach frame past 1 MiB", "its own transport refuses it", Noted::Below),
        ],
        Wire::ReachLink => [
            row(Unknown, "in the link, an Ask of a variant past Ask's", "Say::Unread, unknown, and the link ends", Noted::As(w, U)),
            row(Malformed, "an Ask::Run mangled past its index", "Say::Unread, malformed, and the link ends", Noted::As(w, M)),
            row(Short, "an Ask::Ping cut short", "Say::Unread, malformed: cut short, and the link ends", Noted::As(w, M)),
            row(LeftOver, "an Ask::Ping with 3 bytes after it", "Say::Unread, malformed: 3 bytes left over, and the link ends", Noted::As(w, M)),
            row(TooLarge, "a message whose head says more than 4 MiB", "Say::Unread, malformed: over the limit, and the link ends", Noted::As(w, M)),
        ],
        Wire::SessionUp => up(w, "a nack: unsupported for an unknown command, malformed (its id null) for the rest"),
        Wire::SessionDown => down(w),
        Wire::TerminalUp => up(w, "a refused frame: unknown for an unknown tag, malformed for the rest"),
        Wire::TerminalDown => down(w),
        Wire::StreamUp => up(w, "a refused frame: unknown for an unknown tag, malformed for the rest"),
        Wire::StreamDown => down(w),
        Wire::EventsUp => up(w, "a refused frame: unknown for an unknown tag, malformed for the rest"),
        Wire::EventsDown => down(w),
        Wire::LogUp => up(w, "a refused frame: unknown for an unknown tag, malformed for the rest"),
        Wire::LogDown => down(w),
        Wire::TunnelUp => up(w, "a refused frame: unknown for an unknown kind, malformed for the rest"),
        Wire::TunnelDown => down(w),
        Wire::McpHttp => [
            row(Unknown, "a request for method teleport", "-32601", Noted::As(w, U)),
            row(Malformed, "a request without a method", "-32600", Noted::As(w, M)),
            row(Short, "a request cut short", "-32700", Noted::As(w, M)),
            row(LeftOver, "a request with more after it", "-32700", Noted::As(w, M)),
            row(TooLarge, "a body past 16 MiB", "413, -32600", Noted::As(w, M)),
        ],
        Wire::McpReach => [
            row(Unknown, "a request for method teleport", "-32601; the server hears of it", Noted::As(w, U)),
            row(Malformed, "a request without a method", "-32600; the server hears of it", Noted::As(w, M)),
            row(Short, "a request cut short", "-32700; the server hears of it", Noted::As(w, M)),
            row(LeftOver, "a request with more after it", "-32700; the server hears of it", Noted::As(w, M)),
            row(TooLarge, "a line past 16 MiB", "-32600; the server hears of it", Noted::As(w, M)),
        ],
    }
}

/// The test that sends a wire's cases.
fn sent_by(w: Wire) -> &'static str {
    match w {
        Wire::Runner | Wire::RunnerItem | Wire::RunnerPayload | Wire::Relay => "what_the_server_reads_of_a_runners",
        Wire::Server | Wire::ServerItem | Wire::ServerPayload | Wire::McpHttp | Wire::McpReach => "what_a_runner_reads_of_the_servers_and_mcp",
        Wire::ReachLink => "what_a_runner_reads_in_a_reach_link",
        Wire::SessionUp
        | Wire::SessionDown
        | Wire::TerminalUp
        | Wire::TerminalDown
        | Wire::StreamUp
        | Wire::StreamDown
        | Wire::EventsUp
        | Wire::EventsDown
        | Wire::LogUp
        | Wire::LogDown
        | Wire::TunnelUp
        | Wire::TunnelDown => "what_the_uis_sockets_read",
    }
}

/// Every wire in the inventory has its five cases, one of each, and a test that sends them: the
/// count of those without is printed, and must be 0.
#[test]
fn every_wire_has_its_cases() {
    let mut without = Vec::new();
    for w in WIRES {
        let rows = rows(w);
        let mut cases: Vec<Case> = rows.iter().map(|r| r.case).collect();
        cases.sort();
        if cases != CASES {
            without.push(w.name());
        }
        say(&format!("{} (sent by {}):", w.name(), sent_by(w)));
        for r in &rows {
            let noted = match r.noted {
                Noted::As(on, code) => format!("noted as {} {}", on.name(), code.name()),
                Noted::Below => "never read: counted below the channel".to_owned(),
            };
            say(&format!("  {}: {} → {}; {noted}", r.case.name(), r.sent, r.told));
        }
    }
    say(&format!("wires in the inventory without their cases: {} of {}", without.len(), WIRES.len()));
    assert!(without.is_empty(), "wires without their five cases: {without:?}");
}

// -------------------------------------------------------------------------------------- noting

/// What a case passed, by wire, for the summary each test prints; and how many refusals each
/// sender's audit rows must account for, by wire and code.
#[derive(Default)]
struct Ran {
    passed: BTreeMap<Wire, Vec<Case>>,
    audits: BTreeMap<(String, &'static str, &'static str), u64>,
}

impl Ran {
    /// Case `r` of `w` held, sent by `principal`: noted as `noted` says (or, never read, counted
    /// below the channel), and the next valid message as `next` says.
    fn passed(&mut self, w: Wire, r: &Row, principal: &str, noted: &str, next: &str) {
        let noted = match r.noted {
            Noted::As(on, code) => {
                *self.audits.entry((principal.to_owned(), on.name(), code.name())).or_default() += 1;
                noted
            }
            Noted::Below => "never read: counted `refused` below the channel, on its runner's page",
        };
        self.passed.entry(w).or_default().push(r.case);
        say(&format!("{} {}: {} → {}; {noted}; {next}", w.name(), r.case.name(), r.sent, r.told));
    }

    /// Each sender's audit rows account for each of its refusals: the first of a wire and code in
    /// a minute at once, the rest in the minute's row saying how many.
    fn audited(&self, s: &Server) -> anyhow::Result<()> {
        for ((principal, target, result), n) in &self.audits {
            let deadline = Instant::now() + Duration::from_secs(180);
            loop {
                let rows = audited(s.port, &s.admin, principal);
                let mine: Vec<&Value> = rows.iter().filter(|r| r.get("target").and_then(Value::as_str) == Some(target) && r.get("result").and_then(Value::as_str) == Some(result)).collect();
                let counted: u64 = mine.iter().map(|r| r.get("detail").and_then(Value::as_str).and_then(|d| d.split_once(" more in the minute")).and_then(|(k, _)| k.parse().ok()).unwrap_or(1)).sum();
                if counted >= *n {
                    anyhow::ensure!(counted == *n, "{principal}'s audit rows of {target} {result} say {counted}, not {n}: {mine:?}");
                    break;
                }
                anyhow::ensure!(Instant::now() < deadline, "{principal}'s audit rows of {target} {result} say {counted} of {n}: {rows:?}");
                std::thread::sleep(Duration::from_secs(1));
            }
            say(&format!("audited: {principal}, {target} {result}: {n}"));
        }
        Ok(())
    }

    /// Every case of each of `wires` passed.
    fn covers(&self, wires: &[Wire]) -> anyhow::Result<()> {
        for w in wires {
            let mut ran = self.passed.get(w).cloned().unwrap_or_default();
            ran.sort();
            anyhow::ensure!(ran == CASES, "{}: of its cases, {ran:?} ran", w.name());
            say(&format!("{}: all five cases held", w.name()));
        }
        Ok(())
    }
}

/// A page refusals are kept for: a runner's, or an agent session's.
#[derive(Debug, Clone)]
enum Page {
    Runner(String),
    Session(String),
}

impl Page {
    fn read(&self, s: &Server) -> Value {
        match self {
            Page::Runner(name) => record(s.port, &s.admin, name),
            Page::Session(id) => http(s.port, "GET", &format!("/api/harness/sessions/{id}"), &s.admin, None).ok().and_then(|(_, b)| serde_json::from_str(&b).ok()).unwrap_or_default(),
        }
    }
}

/// What the server had of a case's noting before it was sent.
struct Before {
    counted: u64,
    kept: Vec<u64>,
}

impl Before {
    fn of(s: &Server, noted: Noted, pages: &[Page]) -> Before {
        match noted {
            Noted::As(on, code) => Before { counted: refused(s.port, &s.admin, on.name(), code.name()), kept: pages.iter().map(|p| kept(&p.read(s), on.name(), code.name())).collect() },
            Noted::Below => Before { counted: 0, kept: Vec::new() },
        }
    }

    /// It was noted once: counted, and kept for each of `pages`.
    fn noted_once(&self, s: &Server, noted: Noted, pages: &[Page]) -> anyhow::Result<()> {
        let Noted::As(on, code) = noted else { return Ok(()) };
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let counted = refused(s.port, &s.admin, on.name(), code.name());
            let kept_now: Vec<u64> = pages.iter().map(|p| kept(&p.read(s), on.name(), code.name())).collect();
            let each_once = kept_now.iter().zip(&self.kept).all(|(now, was)| *now == was + 1);
            if counted == self.counted + 1 && each_once {
                return Ok(());
            }
            anyhow::ensure!(counted <= self.counted + 1, "counted {} times", counted - self.counted);
            anyhow::ensure!(Instant::now() < deadline, "{} {}: counted {} (was {}), kept {kept_now:?} (was {:?}) on {pages:?}", on.name(), code.name(), counted, self.counted, self.kept);
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

/// Waits up to a minute for `cond`, saying `what` if it never holds.
fn until(what: &str, mut cond: impl FnMut() -> bool) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !cond() {
        anyhow::ensure!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

/// How many frames of `lane` a page's list of what was let go below the channel says were
/// `refused` (a runner's record's `dropped`, or `dropped_toward`).
fn refused_below(rec: &Value, list: &str, lane: &str) -> u64 {
    rec.get(list).and_then(Value::as_array).into_iter().flatten().filter(|d| d.get("lane").and_then(Value::as_str) == Some(lane) && d.get("reason").and_then(Value::as_str) == Some("refused")).filter_map(|d| d.get("frames").and_then(Value::as_u64)).sum()
}

// ------------------------------------------------------------------------------ what's sent

/// A frame whose variant index is one past an enum of `variants` (a kind its reader doesn't know).
fn past(variants: usize) -> anyhow::Result<Vec<u8>> {
    Ok(vec![u8::try_from(variants)?])
}

/// A frame of variant `index` with what follows it mangled (`kiln::net::MANGLED`): no message
/// reads as it.
fn mangled(index: u8) -> Vec<u8> {
    let mut f = vec![index];
    f.extend_from_slice(&kiln::net::MANGLED);
    f
}

/// `frame` cut to half.
fn short(frame: &[u8]) -> Vec<u8> {
    frame.get(..frame.len() / 2).unwrap_or_default().to_vec()
}

/// `frame` with three bytes after it.
fn left_over(frame: &[u8]) -> Vec<u8> {
    let mut f = frame.to_vec();
    f.extend_from_slice(&[0xde, 0xad, 0xbe]);
    f
}

/// A message's variant index, its first byte.
fn index_of(frame: &[u8]) -> u8 {
    frame.first().copied().unwrap_or(0)
}

/// A runner's battery, `percent` full: what a runner sends that changes its page at once.
fn power(percent: u8) -> AgentMsg {
    AgentMsg::Power { json: json!({"percent": percent, "ac": true, "charging": false}).to_string() }
}

// --------------------------------------------------------------------------- the test's runner

/// A runner of the test's own, over kiln's net layer: it sends what it's given byte for byte, and
/// keeps what the server sends it.
struct Peer {
    name: String,
    net: NetHandle,
    events: async_channel::Receiver<NetEvent>,
    got: Vec<ServerMsg>,
}

impl Peer {
    /// Runner `name` of the server under `dir` (its certificate from the server's CA), connected,
    /// saying hello and what it can do (`build`'s 32 bits, then `caps`), and welcomed.
    fn join(dir: &Path, s: &Server, name: &str, build: u32, caps: u64) -> anyhow::Result<Peer> {
        let pki = kiln::pki::Pki::open(&dir.join("data/pki"))?;
        let leaf = pki.issue_local(name)?;
        let (net, events) = kiln::net::start(kiln::net::Spec { bind: "127.0.0.1:0".parse()?, ca_der: pki.ca_der.clone(), leaf, server: false, dial_deadline: kiln::net::DIAL_DEADLINE, revoked_keys: Vec::new() })?;
        let addr: SocketAddr = format!("127.0.0.1:{}", s.quic).parse()?;
        let dial = net.clone();
        kiln::rt::block_on(async move {
            dial.command(Out::Connect { addr }).await;
            Ok(())
        })?;
        let mut p = Peer { name: name.to_owned(), net, events, got: Vec::new() };
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            match p.events.try_recv() {
                Ok(NetEvent::Connected { .. }) => break,
                Ok(_) => {}
                Err(_) => {
                    anyhow::ensure!(Instant::now() < deadline, "{name} never connected");
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
        let hello = proto::Hello {
            protocol: proto::VERSION,
            name: name.to_owned(),
            labels: Vec::new(),
            slots: 1,
            hostname: name.to_owned(),
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            ephemeral: false,
            dedicated_job: None,
            running: Vec::new(),
            terminal: false,
        };
        p.msg(proto::LANE_CONTROL, &AgentMsg::Hello(hello))?;
        p.msg(proto::LANE_CONTROL, &AgentMsg::Build { build: "hostile".to_owned(), caps: build })?;
        p.msg(proto::LANE_CONTROL, &AgentMsg::Caps { caps })?;
        p.until(|m| matches!(m, ServerMsg::Welcome { .. }))?;
        Ok(p)
    }

    /// `data`, as it is, on `lane`.
    fn send(&self, lane: u32, data: Vec<u8>) -> anyhow::Result<()> {
        let kind = if lane == proto::LANE_CONTROL { Kind::Control } else { Kind::Term };
        anyhow::ensure!(self.net.try_send(SERVER_PEER, lane, kind, data), "{}'s net layer is full", self.name);
        Ok(())
    }

    fn msg(&self, lane: u32, m: &AgentMsg) -> anyhow::Result<()> {
        self.send(lane, proto::encode(m)?)
    }

    /// What the server sent it since it last looked.
    fn take(&mut self) {
        while let Ok(ev) = self.events.try_recv() {
            if let NetEvent::Frame { data, .. } = ev
                && let Ok(m) = proto::read_server(&data)
            {
                self.got.push(m);
            }
        }
    }

    /// The first message from the server `want` takes, waited for up to a minute, and taken.
    fn until(&mut self, want: impl Fn(&ServerMsg) -> bool) -> anyhow::Result<ServerMsg> {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            self.take();
            if let Some(at) = self.got.iter().position(&want) {
                return Ok(self.got.remove(at));
            }
            anyhow::ensure!(Instant::now() < deadline, "{} never got what it waited for, only {:?}", self.name, self.got.iter().map(kiln::wire::server_name).collect::<Vec<_>>());
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A frame of its own on `lane` past its bound: its transport refused it (this process
    /// counts it `refused`), and it says so as a runner does (`AgentMsg::NetDrops`), so its page
    /// shows it.
    fn refused_below(&self, s: &Server, lane: u32, before: u64) -> anyhow::Result<()> {
        let lane_name = proto::lane_name(lane);
        let mine = || kiln::metrics::net().counts().iter().filter(|d| d.lane == lane_name && d.reason == "refused").map(|d| d.frames).sum::<u64>();
        until(&format!("{}'s transport never refused its frame past the bound", self.name), || mine() > before)?;
        self.msg(proto::LANE_CONTROL, &AgentMsg::NetDrops { dropped: kiln::metrics::net().counts() })?;
        until(&format!("{}'s page never said its transport refused a frame", self.name), || refused_below(&record(s.port, &s.admin, &self.name), "dropped", lane_name) > before)?;
        let gauge = metric(s.port, &s.admin, &format!("kiln_runner_net_frames_dropped{{runner=\"{}\",lane=\"{lane_name}\",reason=\"refused\"}}", self.name));
        anyhow::ensure!(gauge > before, "kiln_runner_net_frames_dropped of {} says {gauge}", self.name);
        Ok(())
    }
}

/// How many frames of `lane` this process's transport refused so far.
fn refused_here(lane: u32) -> u64 {
    let lane = proto::lane_name(lane);
    kiln::metrics::net().counts().iter().filter(|d| d.lane == lane && d.reason == "refused").map(|d| d.frames).sum()
}

// -------------------------------------------------------- what the server reads of a runner's

/// A runner's frames, its channel's items, the JSON they carry and its relay frames, each sent
/// what the server can't read, by runners of the test's own.
#[test]
fn what_the_server_reads_of_a_runners() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = common::start_env(dir.path(), &Options::default(), false, &[])?;
    let mut ran = Ran::default();
    runner_frames(dir.path(), &s, &mut ran)?;
    runner_items(dir.path(), &s, &mut ran)?;
    runner_payloads(dir.path(), &s, &mut ran)?;
    relay(dir.path(), &s, &mut ran)?;
    ran.audited(&s)?;
    ran.covers(&[Wire::Runner, Wire::RunnerItem, Wire::RunnerPayload, Wire::Relay])
}

fn runner_frames(dir: &Path, s: &Server, ran: &mut Ran) -> anyhow::Result<()> {
    let name = "hostile-frames";
    let mut p = Peer::join(dir, s, name, 0, proto::CAP_WIRE | proto::CAP_UNREAD)?;
    let pages = [Page::Runner(name.to_owned())];
    let valid = proto::encode(&power(1))?;
    for (n, r) in rows(Wire::Runner).iter().enumerate() {
        let frame = match r.case {
            Case::Unknown => past(kiln::wire::agent_classes().len())?,
            Case::Malformed => mangled(index_of(&valid)),
            Case::Short => short(&valid),
            Case::LeftOver => left_over(&valid),
            Case::TooLarge => proto::encode(&AgentMsg::Power { json: "x".repeat(LARGE) })?,
        };
        let (before, below) = (Before::of(s, r.noted, &pages), refused_here(proto::LANE_CONTROL));
        p.send(proto::LANE_CONTROL, frame)?;
        match r.noted {
            Noted::As(wire, code) => {
                let told = p.until(|m| matches!(m, ServerMsg::Unread { wire: w, code: c, .. } if *w == wire && *c == code))?;
                if r.case == Case::Short {
                    anyhow::ensure!(matches!(&told, ServerMsg::Unread { why, .. } if why == "cut short"), "{told:?}");
                }
                if r.case == Case::LeftOver {
                    anyhow::ensure!(matches!(&told, ServerMsg::Unread { why, .. } if why == "3 bytes left over"), "{told:?}");
                }
            }
            Noted::Below => p.refused_below(s, proto::LANE_CONTROL, below)?,
        }
        before.noted_once(s, r.noted, &pages)?;
        // The next valid frame on the lane, applied.
        let percent = 10 + u8::try_from(n)?;
        p.msg(proto::LANE_CONTROL, &power(percent))?;
        until(&format!("{name}'s next frame was never applied"), || record(s.port, &s.admin, name).pointer("/battery/percent").and_then(Value::as_u64) == Some(u64::from(percent)))?;
        ran.passed(Wire::Runner, r, &format!("runner:{name}"), "counted, audited, on its page", "the next frame applied");
    }
    Ok(())
}

fn runner_items(dir: &Path, s: &Server, ran: &mut Ran) -> anyhow::Result<()> {
    let name = "hostile-items";
    let mut p = Peer::join(dir, s, name, 0, proto::CAP_WIRE | proto::CAP_UNREAD)?;
    let pages = [Page::Runner(name.to_owned())];
    let stream = "hostile";
    let valid = proto::encode(&power(1))?;
    let mut seq = 0;
    for (n, r) in rows(Wire::RunnerItem).iter().enumerate() {
        let body = match r.case {
            Case::Unknown => past(kiln::wire::agent_classes().len())?,
            Case::Malformed => mangled(index_of(&valid)),
            Case::Short => short(&valid),
            Case::LeftOver => left_over(&valid),
            Case::TooLarge => proto::encode(&AgentMsg::Power { json: "x".repeat(LARGE) })?,
        };
        let (before, below) = (Before::of(s, r.noted, &pages), refused_here(proto::LANE_TERM));
        seq += 1;
        p.msg(proto::LANE_TERM, &AgentMsg::Wire(Frame::Item { stream: stream.to_owned(), seq, low: seq - 1, body }))?;
        match r.noted {
            Noted::As(_, code) => {
                let nack = if code == Code::Unknown { Refusal::Unsupported } else { Refusal::Malformed };
                p.until(|m| matches!(m, ServerMsg::Wire(Frame::Ack(h)) if h.stream == stream && h.nacks.iter().any(|k| k.seq == seq && k.code == nack)))?;
            }
            Noted::Below => {
                p.refused_below(s, proto::LANE_TERM, below)?;
                // Never sent: the server never had its number, and the next item takes it.
                seq -= 1;
            }
        }
        before.noted_once(s, r.noted, &pages)?;
        // The next valid item, taken and applied.
        seq += 1;
        let percent = 20 + u8::try_from(n)?;
        p.msg(proto::LANE_TERM, &AgentMsg::Wire(Frame::Item { stream: stream.to_owned(), seq, low: seq - 1, body: proto::encode(&power(percent))? }))?;
        p.until(|m| matches!(m, ServerMsg::Wire(Frame::Ack(h)) if h.stream == stream && h.upto >= seq && !h.nacks.iter().any(|k| k.seq == seq)))?;
        until(&format!("{name}'s next item was never applied"), || record(s.port, &s.admin, name).pointer("/battery/percent").and_then(Value::as_u64) == Some(u64::from(percent)))?;
        ran.passed(Wire::RunnerItem, r, &format!("runner:{name}"), "counted, audited, on its page", "the next item taken and applied");
    }
    Ok(())
}

fn runner_payloads(dir: &Path, s: &Server, ran: &mut Ran) -> anyhow::Result<()> {
    let name = "hostile-payloads";
    let mut p = Peer::join(dir, s, name, 0, proto::CAP_WIRE | proto::CAP_UNREAD)?;
    let pages = [Page::Runner(name.to_owned())];
    for (n, r) in rows(Wire::RunnerPayload).iter().enumerate() {
        let req = 100 + u64::try_from(n)?;
        let json = match r.case {
            Case::Unknown => r#"{"op":"teleport"}"#.to_owned(),
            Case::Malformed => r#"{"op":"call","name":7}"#.to_owned(),
            Case::Short => r#"{"op":"direc"#.to_owned(),
            Case::LeftOver => r#"{"op":"directory"} {"op":"directory"}"#.to_owned(),
            Case::TooLarge => json!({"op": "directory", "pad": "x".repeat(LARGE)}).to_string(),
        };
        let (before, below) = (Before::of(s, r.noted, &pages), refused_here(proto::LANE_CONTROL));
        p.msg(proto::LANE_CONTROL, &AgentMsg::ReachAsk { req, json })?;
        match r.noted {
            Noted::As(wire, code) => {
                p.until(|m| matches!(m, ServerMsg::Unread { wire: w, code: c, .. } if *w == wire && *c == code))?;
                // What it asked is answered too, saying why.
                let answer = p.until(|m| matches!(m, ServerMsg::ReachAnswer { req: q, .. } if *q == req))?;
                let ServerMsg::ReachAnswer { json, .. } = answer else { anyhow::bail!("not an answer") };
                let said: Value = serde_json::from_str(&json)?;
                anyhow::ensure!(said.get("error").is_some() && said.get("code").and_then(Value::as_str) == Some(code.name()), "its question was answered {said}");
            }
            Noted::Below => p.refused_below(s, proto::LANE_CONTROL, below)?,
        }
        before.noted_once(s, r.noted, &pages)?;
        // The next valid question, answered.
        let next = 200 + u64::try_from(n)?;
        p.msg(proto::LANE_CONTROL, &AgentMsg::ReachAsk { req: next, json: r#"{"op":"directory"}"#.to_owned() })?;
        let answer = p.until(|m| matches!(m, ServerMsg::ReachAnswer { req: q, .. } if *q == next))?;
        anyhow::ensure!(matches!(&answer, ServerMsg::ReachAnswer { json, .. } if json.contains("\"runners\"")), "{answer:?}");
        ran.passed(Wire::RunnerPayload, r, &format!("runner:{name}"), "counted, audited, on its page", "the next question answered");
    }
    Ok(())
}

fn relay(dir: &Path, s: &Server, ran: &mut Ran) -> anyhow::Result<()> {
    // Two runners with a link between them, and a third that sends on it.
    let (build, caps) = (proto::CAP_REACH, proto::CAP_WIRE | proto::CAP_UNREAD);
    let mut a = Peer::join(dir, s, "hostile-relay-a", build, caps)?;
    let b = Peer::join(dir, s, "hostile-relay-b", build, caps)?;
    let mut c = Peer::join(dir, s, "hostile-relay-c", build, caps)?;
    for p in [&a, &b, &c] {
        p.msg(proto::LANE_CONTROL, &AgentMsg::ReachHello { json: json!({"fingerprint": format!("fp-{}", p.name), "accept": true, "version": 1}).to_string() })?;
    }
    until("the relay never knew the runners", || {
        let (_, body) = http(s.port, "GET", "/api/reach/runners", &s.admin, None).unwrap_or_default();
        ["a", "b", "c"].iter().all(|n| body.contains(&format!("fp-hostile-relay-{n}")))
    })?;
    b.msg(proto::LANE_REACH, &AgentMsg::Reach { to: a.name.clone(), link: "L1".into(), open: true, data: b"hello".to_vec(), close: None })?;
    a.until(|m| matches!(m, ServerMsg::Reach { from, link, .. } if from == "hostile-relay-b" && link == "L1"))?;
    let pages = [Page::Runner(c.name.clone())];
    for (n, r) in rows(Wire::Relay).iter().enumerate() {
        let valid = proto::encode(&AgentMsg::Reach { to: a.name.clone(), link: format!("C{n}"), open: true, data: b"hi".to_vec(), close: None })?;
        let frame = match r.case {
            Case::Unknown => past(kiln::wire::agent_classes().len())?,
            Case::Malformed => proto::encode(&AgentMsg::Reach { to: a.name.clone(), link: "L1".into(), open: false, data: b"rm -rf /".to_vec(), close: None })?,
            Case::Short => short(&valid),
            Case::LeftOver => left_over(&valid),
            Case::TooLarge => proto::encode(&AgentMsg::Reach { to: a.name.clone(), link: format!("C{n}"), open: true, data: vec![0; LARGE], close: None })?,
        };
        let (before, below) = (Before::of(s, r.noted, &pages), refused_here(proto::LANE_REACH));
        c.send(proto::LANE_REACH, frame)?;
        match r.noted {
            Noted::As(wire, code) => {
                let told = c.until(|m| matches!(m, ServerMsg::Unread { wire: w, code: k, .. } if *w == wire && *k == code))?;
                if wire == Wire::Relay {
                    anyhow::ensure!(matches!(&told, ServerMsg::Unread { why, .. } if why.contains("not its link") && why.contains("hostile-relay-a") && why.contains("hostile-relay-b")), "{told:?}");
                }
            }
            Noted::Below => c.refused_below(s, proto::LANE_REACH, below)?,
        }
        before.noted_once(s, r.noted, &pages)?;
        // The next valid frame: a link of its own, which reaches its other end.
        let link = format!("N{n}");
        c.msg(proto::LANE_REACH, &AgentMsg::Reach { to: a.name.clone(), link: link.clone(), open: true, data: b"next".to_vec(), close: None })?;
        a.until(|m| matches!(m, ServerMsg::Reach { from, link: l, .. } if from == "hostile-relay-c" && *l == link))?;
        ran.passed(Wire::Relay, r, &format!("runner:{}", c.name), "counted, audited, on its page", "its next frame relayed");
    }
    a.take();
    anyhow::ensure!(!a.got.iter().any(|m| matches!(m, ServerMsg::Reach { data, .. } if data == b"rm -rf /")), "what was sent on another pair's link reached its end");
    Ok(())
}

// ------------------------------------------- what a runner reads of the server's, and MCP

/// A real runner of the server, with an agent session (the stand-in Claude Code): its name, its
/// log, the session's id and the folder it works in.
struct Real {
    _runner: agents::Runner,
    name: String,
    log: PathBuf,
    session: String,
    work: PathBuf,
}

impl Real {
    fn start(dir: &Path, s: &Server, name: &str) -> anyhow::Result<Real> {
        let (runner, log) = agents::runner_with(dir, s.port, name, None, Talk { lines: 1, every_ms: 10 }, &[])?;
        agents::connected(s.port, &s.admin, name, &log, Duration::from_secs(120))?;
        let work = dir.join(format!("{name}-session"));
        std::fs::create_dir_all(&work)?;
        std::fs::write(work.join(".fake-claude-controls"), "")?;
        let session = agents::open_session(s.port, &s.admin, name, &work, Duration::from_secs(180))?;
        Ok(Real { _runner: runner, name: name.to_owned(), log, session, work })
    }

    fn says(&self, what: &str) -> usize {
        std::fs::read_to_string(&self.log).unwrap_or_default().matches(what).count()
    }

    /// A valid command to its session (a model to change to), applied there.
    fn applied(&self, s: &Server, model: &str) -> anyhow::Result<()> {
        let path = format!("/api/harness/sessions/{}/command", self.session);
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let (st, body) = http(s.port, "POST", &path, &s.admin, Some(&json!({"type": "set_model", "model": model})))?;
            if (200..300).contains(&st) {
                break;
            }
            anyhow::ensure!(Instant::now() < deadline, "set_model {model}: {st} {body}");
            std::thread::sleep(Duration::from_secs(1));
        }
        until(&format!("{model} never reached the session"), || std::fs::read_to_string(self.work.join(".fake-claude-controls")).unwrap_or_default().contains(model))
    }
}

/// `bytes` sent to runner `runner` by the server as a frame on `lane`, or as an item of its live
/// command stream (`item`, answered).
fn chaos_send(s: &Server, runner: &str, lane: &str, item: bool, bytes: &[u8]) -> anyhow::Result<Value> {
    let bytes = base64::engine::general_purpose::STANDARD.encode(bytes);
    let (st, body) = http(s.port, "POST", "/api/chaos/send", &s.admin, Some(&json!({"runner": runner, "lane": lane, "item": item, "bytes": bytes})))?;
    anyhow::ensure!(st == 200, "chaos send: {st} {body}");
    Ok(serde_json::from_str(&body)?)
}

/// What the server's transport refused of its own frames on the control lane: its count, and runner
/// `name`'s page's.
fn server_refused(s: &Server, name: &str) -> (u64, u64) {
    (metric(s.port, &s.admin, "kiln_net_frames_dropped_total{lane=\"control\",reason=\"refused\"}"), refused_below(&record(s.port, &s.admin, name), "dropped_toward", "control"))
}

/// The server's frames, the items of its command streams and the JSON they carry, each sent to a
/// real runner as the server never would; and MCP, over HTTP and to the runner's own MCP server.
#[test]
fn what_a_runner_reads_of_the_servers_and_mcp() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = common::start_env(dir.path(), &Options::default(), false, &[])?;
    let real = Real::start(dir.path(), &s, &format!("hostile-real-{}", std::process::id()))?;
    let mut ran = Ran::default();
    server_frames(&s, &real, &mut ran)?;
    server_items(&s, &real, &mut ran)?;
    server_payloads(&s, &real, &mut ran)?;
    mcp_http(&s, &real, &mut ran)?;
    mcp_reach(dir.path(), &s, &real, &mut ran)?;
    ran.audited(&s)?;
    ran.covers(&[Wire::Server, Wire::ServerItem, Wire::ServerPayload, Wire::McpHttp, Wire::McpReach])
}

fn server_frames(s: &Server, real: &Real, ran: &mut Ran) -> anyhow::Result<()> {
    let pages = [Page::Runner(real.name.clone())];
    let resize = proto::encode(&ServerMsg::TermResize { session: "no-such-terminal".into(), cols: 80, rows: 24 })?;
    let cancel = proto::encode(&ServerMsg::Cancel { job: "no-such-job".into(), reason: "hostile".into() })?;
    for (n, r) in rows(Wire::Server).iter().enumerate() {
        let frame = match r.case {
            Case::Unknown => past(kiln::wire::server_classes().len())?,
            Case::Malformed => mangled(index_of(&resize)),
            Case::Short => short(&cancel),
            Case::LeftOver => left_over(&cancel),
            Case::TooLarge => proto::encode(&ServerMsg::TermResize { session: "x".repeat(LARGE), cols: 80, rows: 24 })?,
        };
        let (before, below, said) = (Before::of(s, r.noted, &pages), server_refused(s, &real.name), real.says("a frame from the server couldn't be read"));
        chaos_send(s, &real.name, "control", false, &frame)?;
        match r.noted {
            // The runner couldn't read it, and says so: the server notes it against the runner.
            Noted::As(..) => until("the runner never said it couldn't read the frame", || real.says("a frame from the server couldn't be read") > said)?,
            Noted::Below => until("the server's transport never refused its frame past the bound", || {
                let now = server_refused(s, &real.name);
                now.0 > below.0 && now.1 > below.1
            })?,
        }
        before.noted_once(s, r.noted, &pages)?;
        real.applied(s, &format!("hostile-server-{n}"))?;
        ran.passed(Wire::Server, r, &format!("runner:{}", real.name), "the runner said so; counted, audited, on its page", "the next frame applied");
    }
    Ok(())
}

fn server_items(s: &Server, real: &Real, ran: &mut Ran) -> anyhow::Result<()> {
    let pages = [Page::Runner(real.name.clone())];
    let resize = proto::encode(&ServerMsg::TermResize { session: "no-such-terminal".into(), cols: 80, rows: 24 })?;
    for (n, r) in rows(Wire::ServerItem).iter().enumerate() {
        let body = match r.case {
            Case::Unknown => past(kiln::wire::server_classes().len())?,
            Case::Malformed => mangled(index_of(&resize)),
            Case::Short => short(&resize),
            Case::LeftOver => left_over(&resize),
            Case::TooLarge => proto::encode(&ServerMsg::TermResize { session: "x".repeat(LARGE), cols: 80, rows: 24 })?,
        };
        let (before, below) = (Before::of(s, r.noted, &pages), server_refused(s, &real.name));
        let answer = chaos_send(s, &real.name, "control", true, &body)?;
        match r.noted {
            Noted::As(_, code) => {
                let nack = if code == Code::Unknown { "unsupported" } else { "malformed" };
                anyhow::ensure!(answer.get("answer").and_then(Value::as_str) == Some("refused") && answer.get("code").and_then(Value::as_str) == Some(nack), "answered {answer}");
            }
            Noted::Below => {
                anyhow::ensure!(answer.get("answer").and_then(Value::as_str) == Some("unreachable") && answer.get("why").and_then(Value::as_str).is_some_and(|w| w.contains("too large")), "answered {answer}");
                let now = server_refused(s, &real.name);
                anyhow::ensure!(now.0 > below.0 && now.1 > below.1, "not counted as refused: {below:?} then {now:?}");
            }
        }
        before.noted_once(s, r.noted, &pages)?;
        real.applied(s, &format!("hostile-item-{n}"))?;
        ran.passed(Wire::ServerItem, r, &format!("runner:{}", real.name), "the command answered so; counted, audited, on its page", "the next command applied");
    }
    Ok(())
}

fn server_payloads(s: &Server, real: &Real, ran: &mut Ran) -> anyhow::Result<()> {
    let pages = [Page::Runner(real.name.clone()), Page::Session(real.session.clone())];
    for (n, r) in rows(Wire::ServerPayload).iter().enumerate() {
        let json = match r.case {
            Case::Unknown => r#"{"type":"teleport"}"#.to_owned(),
            Case::Malformed => r#"{"type":"set_model","model":7}"#.to_owned(),
            Case::Short => r#"{"type":"set_model","mod"#.to_owned(),
            Case::LeftOver => r#"{"type":"set_model","model":"x"} {}"#.to_owned(),
            Case::TooLarge => json!({"type": "set_model", "model": "x".repeat(LARGE)}).to_string(),
        };
        let body = proto::encode(&ServerMsg::HarnessCmd { session: real.session.clone(), json })?;
        let (before, below) = (Before::of(s, r.noted, &pages), server_refused(s, &real.name));
        let answer = chaos_send(s, &real.name, "control", true, &body)?;
        match r.noted {
            Noted::As(_, code) => {
                let nack = if code == Code::Unknown { "unsupported" } else { "malformed" };
                anyhow::ensure!(answer.get("answer").and_then(Value::as_str) == Some("refused") && answer.get("code").and_then(Value::as_str) == Some(nack), "answered {answer}");
            }
            Noted::Below => {
                anyhow::ensure!(answer.get("answer").and_then(Value::as_str) == Some("unreachable") && answer.get("why").and_then(Value::as_str).is_some_and(|w| w.contains("too large")), "answered {answer}");
                let now = server_refused(s, &real.name);
                anyhow::ensure!(now.0 > below.0 && now.1 > below.1, "not counted as refused: {below:?} then {now:?}");
            }
        }
        before.noted_once(s, r.noted, &pages)?;
        real.applied(s, &format!("hostile-payload-{n}"))?;
        ran.passed(Wire::ServerPayload, r, &format!("runner:{}", real.name), "the command answered so; counted, audited, on the runner's and the session's pages", "the next command applied");
    }
    Ok(())
}

/// One HTTP request of a raw body: its status and its body. The body goes on a thread of its own, so
/// an answer that comes before all of it was read (one too large) is read all the same.
fn post_raw(port: u16, path: &str, token: &str, body: Vec<u8>) -> anyhow::Result<(u16, String)> {
    let mut sock = std::net::TcpStream::connect(("127.0.0.1", port))?;
    sock.set_read_timeout(Some(Duration::from_secs(60)))?;
    write!(sock, "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len())?;
    let mut writer = sock.try_clone()?;
    let sending = std::thread::spawn(move || {
        let _ = writer.write_all(&body);
    });
    let mut raw = Vec::new();
    let _ = sock.read_to_end(&mut raw);
    let _ = sending.join();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let rest = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") { common::dechunk(rest) } else { rest.to_owned() };
    Ok((status, rest))
}

/// An MCP request's bytes for each case, as an agent sends it.
fn mcp_bytes(case: Case) -> Vec<u8> {
    match case {
        Case::Unknown => br#"{"jsonrpc":"2.0","id":1,"method":"teleport"}"#.to_vec(),
        Case::Malformed => br#"{"jsonrpc":"2.0","id":2}"#.to_vec(),
        Case::Short => br#"{"jsonrpc":"2.0","id":3,"meth"#.to_vec(),
        Case::LeftOver => br#"{"jsonrpc":"2.0","id":4,"method":"ping"} {}"#.to_vec(),
        Case::TooLarge => vec![b'x'; kiln::mcp::rpc::BODY_MOST + 1],
    }
}

/// The JSON-RPC error code an MCP server answers each case with.
fn mcp_code(case: Case) -> i64 {
    match case {
        Case::Unknown => kiln::mcp::rpc::METHOD_NOT_FOUND,
        Case::Malformed | Case::TooLarge => kiln::mcp::rpc::INVALID_REQUEST,
        Case::Short | Case::LeftOver => kiln::mcp::rpc::PARSE_ERROR,
    }
}

fn mcp_http(s: &Server, real: &Real, ran: &mut Ran) -> anyhow::Result<()> {
    // The agent session's own MCP token: what it sends is noted against it, on its page.
    let token = kiln::mcp::session_token(&s.admin, &real.session);
    let pages = [Page::Session(real.session.clone())];
    for (n, r) in rows(Wire::McpHttp).iter().enumerate() {
        let before = Before::of(s, r.noted, &pages);
        let (st, body) = post_raw(s.port, "/api/mcp", &token, mcp_bytes(r.case))?;
        let answer: Value = serde_json::from_str(&body).unwrap_or_default();
        let want = if r.case == Case::TooLarge { 413 } else { 200 };
        anyhow::ensure!(st == want && answer.pointer("/error/code").and_then(Value::as_i64) == Some(mcp_code(r.case)), "{}: {st} {body}", r.case.name());
        before.noted_once(s, r.noted, &pages)?;
        // The next valid request, answered.
        let ping = json!({"jsonrpc": "2.0", "id": 100 + n, "method": "ping"});
        let (st, body) = http(s.port, "POST", "/api/mcp", &token, Some(&ping))?;
        anyhow::ensure!(st == 200 && body.contains("\"result\""), "ping: {st} {body}");
        ran.passed(Wire::McpHttp, r, &format!("agent:{}", real.session), "counted, audited naming the session, on its page", "the next request answered");
    }
    Ok(())
}

/// The reach socket of runner `name`, in this user's runtime folder for sockets.
fn reach_socket(name: &str) -> anyhow::Result<PathBuf> {
    let probe = kiln::harness::socket::runtime_path("x");
    let folder = probe.parent().map(Path::to_path_buf).unwrap_or_default();
    let prefix = format!("reach-{name}-");
    let found: Vec<PathBuf> = std::fs::read_dir(&folder)?.flatten().map(|e| e.path()).filter(|p| p.file_name().is_some_and(|f| f.to_string_lossy().starts_with(&prefix) && f.to_string_lossy().ends_with(".sock"))).collect();
    match found.as_slice() {
        [one] => Ok(one.clone()),
        _ => anyhow::bail!("runner {name}'s reach socket, in {}: {found:?}", folder.display()),
    }
}

fn mcp_reach(dir: &Path, s: &Server, real: &Real, ran: &mut Ran) -> anyhow::Result<()> {
    // The runner's own MCP server, as its agent session's harness runs it.
    let sock = reach_socket(&real.name)?;
    let mut child = common::kiln()
        .args(["reach", "mcp"])
        .env("KILN_REACH_SOCK", &sock)
        .env("KILN_SESSION", &real.session)
        .env("HOME", dir.join("home"))
        .current_dir(&real.work)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::fs::File::create(dir.join("reach-mcp.log"))?)
        .spawn()?;
    let mut to = child.stdin.take().ok_or_else(|| anyhow::anyhow!("no stdin"))?;
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let from = child.stdout.take().ok_or_else(|| anyhow::anyhow!("no stdout"))?;
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(from).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    // Its answer to a line: the next message that's an answer (it may say other things between).
    let answer = |rx: &std::sync::mpsc::Receiver<String>| -> anyhow::Result<Value> {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = rx.recv_timeout(left).map_err(|_| anyhow::anyhow!("kiln reach mcp never answered: {}", std::fs::read_to_string(dir.join("reach-mcp.log")).unwrap_or_default()))?;
            let v: Value = serde_json::from_str(&line).unwrap_or_default();
            if v.get("result").is_some() || v.get("error").is_some() {
                return Ok(v);
            }
        }
    };
    let pages = [Page::Session(real.session.clone()), Page::Runner(real.name.clone())];
    for (n, r) in rows(Wire::McpReach).iter().enumerate() {
        let before = Before::of(s, r.noted, &pages);
        let mut line = mcp_bytes(r.case);
        line.push(b'\n');
        to.write_all(&line)?;
        to.flush()?;
        let a = answer(&rx)?;
        anyhow::ensure!(a.pointer("/error/code").and_then(Value::as_i64) == Some(mcp_code(r.case)), "{}: {a}", r.case.name());
        before.noted_once(s, r.noted, &pages)?;
        writeln!(to, "{}", json!({"jsonrpc": "2.0", "id": 100 + n, "method": "ping"}))?;
        to.flush()?;
        let a = answer(&rx)?;
        anyhow::ensure!(a.get("result").is_some(), "ping: {a}");
        ran.passed(Wire::McpReach, r, &format!("agent:{}", real.session), "counted, audited naming the session, on its page and its runner's", "the next request answered");
    }
    drop(to);
    let _ = child.kill();
    let _ = child.wait();
    Ok(())
}

// ------------------------------------------------------- what a runner reads in a reach link

/// A link the test's runner opened to a real runner, through the relay, over its end-to-end TLS.
struct Link {
    id: String,
    tls: kiln::reach::e2e::Tls,
    plain: kiln::reach::wire::Deframer,
    closed: Option<String>,
}

impl Link {
    /// A link from `p` (its reach identity `me`) to runner `to` (its key `fingerprint`), its
    /// handshake done.
    fn open(p: &mut Peer, me: &kiln::reach::e2e::Identity, to: &str, fingerprint: &str, id: &str) -> anyhow::Result<Link> {
        let mut l = Link { id: id.to_owned(), tls: kiln::reach::e2e::Tls::client(me, fingerprint)?, plain: kiln::reach::wire::Deframer::default(), closed: None };
        let mut first = true;
        let deadline = Instant::now() + Duration::from_secs(60);
        while !l.tls.ready() {
            let out = l.tls.outgoing();
            if !out.is_empty() || first {
                p.msg(proto::LANE_REACH, &AgentMsg::Reach { to: to.to_owned(), link: id.to_owned(), open: first, data: out, close: None })?;
                first = false;
            }
            l.read(p, to, deadline)?;
            anyhow::ensure!(l.closed.is_none(), "{to} closed link {id}: {:?}", l.closed);
        }
        // Its last handshake flight goes with the first message.
        Ok(l)
    }

    /// What came on the link since, taken in.
    fn read(&mut self, p: &mut Peer, from: &str, deadline: Instant) -> anyhow::Result<()> {
        let id = self.id.clone();
        let m = p.until(|m| matches!(m, ServerMsg::Reach { from: f, link, .. } if f == from && *link == id))?;
        let ServerMsg::Reach { data, close, .. } = m else { anyhow::bail!("not a reach frame") };
        let mut plain = Vec::new();
        self.tls.receive(&data, &mut plain)?;
        self.plain.push(&plain);
        if close.is_some() {
            self.closed = close;
        }
        anyhow::ensure!(Instant::now() < deadline, "link {id} said nothing in time");
        Ok(())
    }

    /// `message`, framed as the link frames its messages (its length, then it), encrypted and sent.
    fn say(&mut self, p: &Peer, to: &str, message: &[u8], head: Option<u32>) -> anyhow::Result<()> {
        let len = head.unwrap_or(u32::try_from(message.len())?);
        let mut framed = len.to_be_bytes().to_vec();
        framed.extend_from_slice(message);
        self.tls.send(&framed)?;
        p.msg(proto::LANE_REACH, &AgentMsg::Reach { to: to.to_owned(), link: self.id.clone(), open: false, data: self.tls.outgoing(), close: None })
    }

    /// The next message the other end says.
    fn heard(&mut self, p: &mut Peer, from: &str) -> anyhow::Result<kiln::reach::wire::Say> {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(say) = self.plain.read::<kiln::reach::wire::Say>(kiln::reach::wire::SAY_VARIANTS).map_err(|e| anyhow::anyhow!("{e:?}"))? {
                return Ok(say);
            }
            self.read(p, from, deadline)?;
        }
    }
}

/// What a real runner reads inside a reach link, sent by the test's runner at its other end: each
/// answered `Say::Unread` before the link ends, and the server told, naming the runner that sent it.
#[test]
fn what_a_runner_reads_in_a_reach_link() -> anyhow::Result<()> {
    use kiln::reach::wire::{ASK_VARIANTS, Ask, MAX_MESSAGE, Say};
    let dir = tempfile::tempdir()?;
    let s = common::start_env(dir.path(), &Options::default(), false, &[])?;
    let a = format!("hostile-reach-{}", std::process::id());
    let (_a, a_log) = agents::runner_with(dir.path(), s.port, &a, None, Talk { lines: 1, every_ms: 10 }, &[])?;
    agents::connected(s.port, &s.admin, &a, &a_log, Duration::from_secs(120))?;
    // The runner at the link's other end, with a reach identity of its own the server knows: all of
    // its links' ends announced before the first opens (a runner trusts the directory it reads then).
    let b = "hostile-reach-b";
    let mut p = Peer::join(dir.path(), &s, b, proto::CAP_REACH, proto::CAP_WIRE | proto::CAP_UNREAD)?;
    let me = kiln::reach::e2e::Identity::load_or_create(&dir.path().join("hostile-reach-b-identity"), b)?;
    p.msg(proto::LANE_CONTROL, &AgentMsg::ReachHello { json: json!({"fingerprint": me.fingerprint, "accept": false, "version": kiln::reach::wire::VERSION}).to_string() })?;
    let mut fingerprint = String::new();
    until("the directory never listed both runners", || {
        let (_, body) = http(s.port, "GET", "/api/reach/runners", &s.admin, None).unwrap_or_default();
        let listed: Value = serde_json::from_str(&body).unwrap_or_default();
        let runners = listed.as_array().cloned().or_else(|| listed.get("runners").and_then(Value::as_array).cloned()).unwrap_or_default();
        let of = |n: &str| runners.iter().find(|r| r.get("name").and_then(Value::as_str) == Some(n)).and_then(|r| r.get("fingerprint").and_then(Value::as_str)).map(str::to_owned);
        if let (Some(fa), Some(_)) = (of(&a), of(b)) {
            fingerprint = fa;
            return true;
        }
        false
    })?;
    let pages = [Page::Runner(b.to_owned()), Page::Runner(a.clone())];
    let ping = postcard::to_allocvec(&Ask::Ping { at: 1_792_900_000_000 })?;
    let run = postcard::to_allocvec(&Ask::Run(Box::default()))?;
    let mut ran = Ran::default();
    for (n, r) in rows(Wire::ReachLink).iter().enumerate() {
        let (message, head) = match r.case {
            Case::Unknown => (past(ASK_VARIANTS)?, None),
            Case::Malformed => (mangled(index_of(&run)), None),
            Case::Short => (short(&ping), None),
            Case::LeftOver => (left_over(&ping), None),
            // Its head alone: the reader refuses it before waiting for the rest.
            Case::TooLarge => (Vec::new(), Some(u32::try_from(MAX_MESSAGE + 1)?)),
        };
        let before = Before::of(&s, r.noted, &pages);
        let mut link = Link::open(&mut p, &me, &a, &fingerprint, &format!("hostile-{n}"))?;
        link.say(&p, &a, &message, head)?;
        let Noted::As(_, code) = r.noted else { anyhow::bail!("a reach link's case is noted") };
        let told = link.heard(&mut p, &a)?;
        anyhow::ensure!(matches!(&told, Say::Unread { code: c, .. } if *c == code), "{told:?}");
        before.noted_once(&s, r.noted, &pages)?;
        // The next valid message, on a link of its own: answered.
        let mut next = Link::open(&mut p, &me, &a, &fingerprint, &format!("next-{n}"))?;
        next.say(&p, &a, &postcard::to_allocvec(&Ask::Hello { version: kiln::reach::wire::VERSION, session: format!("hostile-{n}"), resume: Vec::new() })?, None)?;
        let welcome = next.heard(&mut p, &a)?;
        anyhow::ensure!(matches!(welcome, Say::Welcome { .. }), "{welcome:?}");
        ran.passed(Wire::ReachLink, r, &format!("runner:{b}"), "the link ended; counted, audited naming its sender, on both runners' pages", "the next link welcomed");
    }
    ran.audited(&s)?;
    ran.covers(&[Wire::ReachLink])
}

// ------------------------------------------------------------------------ the UI's sockets

/// A frame for a client to send (masked), text (opcode 1) or binary (2).
fn ws_frame(op: u8, payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut f = vec![0x80 | op];
    match payload.len() {
        n if n < 126 => f.push(0x80 | u8::try_from(n)?),
        n if n < 1 << 16 => {
            f.push(0x80 | 126);
            f.extend(u16::try_from(n)?.to_be_bytes());
        }
        n => {
            f.push(0x80 | 127);
            f.extend(u64::try_from(n)?.to_be_bytes());
        }
    }
    let mask = [0x37u8, 0xfa, 0x21, 0x3d];
    f.extend(mask);
    f.extend(payload.iter().zip(mask.iter().cycle()).map(|(b, m)| b ^ m));
    Ok(f)
}

/// What a UI socket's case sends: up, a frame it can't read, as that socket reads what comes up
/// (a terminal's frames are tagged `type`, the rest `t`; the events' and the tunnel's only JSON
/// frame up is `unread`, and the tunnel reads other text as a kind of its own, `bind …`); down,
/// what a page couldn't read of what came down, said up (step 6's `unread`).
fn ui_frame(w: Wire, case: Case) -> Vec<u8> {
    let down = matches!(w, Wire::SessionDown | Wire::TerminalDown | Wire::StreamDown | Wire::EventsDown | Wire::LogDown | Wire::TunnelDown);
    if down {
        // `of` is the frame's tag when the page could read one (`unknown`), empty when it couldn't
        // (`malformed`).
        let (of, why) = match case {
            Case::Unknown => ("hologram", "unknown frame"),
            Case::Malformed => ("", "not JSON"),
            Case::Short => ("", "cut short"),
            Case::LeftOver => ("", "bytes left over"),
            Case::TooLarge => ("", "too large"),
        };
        return json!({"t": "unread", "of": of, "why": why}).to_string().into_bytes();
    }
    let valid = match w {
        Wire::SessionUp => r#"{"t":"cmd","id":"hostile","c":{"type":"set_model","model":"x"}}"#,
        Wire::TerminalUp => r#"{"type":"resize","cols":80,"rows":24}"#,
        _ => r#"{"t":"unread","of":"","why":"x"}"#,
    };
    match case {
        Case::Unknown if w == Wire::SessionUp => br#"{"t":"cmd","id":"hostile-unknown","c":{"type":"teleport"}}"#.to_vec(),
        Case::Unknown if w == Wire::TerminalUp => br#"{"type":"teleport"}"#.to_vec(),
        Case::Unknown => br#"{"t":"teleport"}"#.to_vec(),
        Case::Malformed if w == Wire::TunnelUp => b"{\x01\x02 not json".to_vec(),
        Case::Malformed => b"\x01\x02 not json".to_vec(),
        Case::Short => short(valid.as_bytes()),
        Case::LeftOver => format!("{valid} x").into_bytes(),
        Case::TooLarge => vec![b' '; (16 << 20) + 1024],
    }
}

/// The pages a UI socket's refusals are kept for: a session's sockets' (its own, its streams'), its
/// session's; a runner's terminal's, the runner's; the rest have none (a person's refusals have no
/// page of their own).
fn ui_pages(w: Wire, real: &Real) -> Vec<Page> {
    match w {
        Wire::SessionUp | Wire::SessionDown | Wire::StreamUp | Wire::StreamDown => vec![Page::Session(real.session.clone())],
        Wire::TerminalUp | Wire::TerminalDown => vec![Page::Runner(real.name.clone())],
        _ => Vec::new(),
    }
}

/// What a UI socket answers a case, as step 6 has it: a frame past its limit closes it, 1009; the
/// session's socket nacks what it can't read (`unsupported` for a command it doesn't know,
/// `malformed` for the rest); the others say `refused`; what a page says it couldn't read is
/// answered with nothing.
fn typed_answer(sock: &mut std::net::TcpStream, w: Wire, r: &Row) -> anyhow::Result<()> {
    if matches!(w, Wire::SessionDown | Wire::TerminalDown | Wire::StreamDown | Wire::EventsDown | Wire::LogDown | Wire::TunnelDown) {
        return Ok(());
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        anyhow::ensure!(Instant::now() < deadline, "{} {}: never answered {}", w.name(), r.case.name(), r.told);
        let (op, payload) = agents::frame(sock).map_err(|e| anyhow::anyhow!("{} {}: waiting for {}: {e:#}", w.name(), r.case.name(), r.told))?;
        if r.case == Case::TooLarge {
            if op == 8 {
                anyhow::ensure!(payload.get(..2) == Some(1009u16.to_be_bytes().as_slice()), "{} {}: closed {payload:?}", w.name(), r.case.name());
                return Ok(());
            }
            continue;
        }
        let Ok(f) = serde_json::from_slice::<Value>(&payload) else { continue };
        // A terminal's and a log's frames are tagged `type`, the rest `t`.
        let (t, code) = (f.get("t").or_else(|| f.get("type")).and_then(Value::as_str), f.get("code").and_then(Value::as_str));
        let unknown = r.case == Case::Unknown;
        let said = match w {
            Wire::SessionUp => t == Some("nack") && code == Some(if unknown { "unsupported" } else { "malformed" }),
            _ => t == Some("refused") && (if unknown { matches!(code, Some("unknown" | "unsupported")) } else { code == Some("malformed") }),
        };
        if said {
            return Ok(());
        }
    }
}

/// The UI's sockets, each way, sent what they can't read: each answered typed, counted, audited
/// naming the person and kept on its page, and the socket goes on after it (after one past its
/// limit, a new one does), the next valid message on it applied. The sockets this test opens are the session's, the events', a runner's terminal and the tunnel;
/// a stream's and a log's need a stream and a job of their own, which step 6's
/// tests/hostile_sockets.rs opens.
#[test]
fn what_the_uis_sockets_read() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = common::start_env(dir.path(), &Options { agent: false, ci: "", extra: "[terminal]\nenabled = true\n" }, false, &[])?;
    let real = Real::start(dir.path(), &s, &format!("hostile-ui-{}", std::process::id()))?;
    let mut ran = Ran::default();
    let sockets: [(Wire, Option<String>); 12] = [
        (Wire::SessionUp, Some(format!("/api/harness/sessions/{}/ws", real.session))),
        (Wire::SessionDown, Some(format!("/api/harness/sessions/{}/ws", real.session))),
        (Wire::TerminalUp, Some(format!("/api/terminal/ws?target=agent:{}", real.name))),
        (Wire::TerminalDown, Some(format!("/api/terminal/ws?target=agent:{}", real.name))),
        (Wire::StreamUp, None),
        (Wire::StreamDown, None),
        (Wire::EventsUp, Some("/api/events".to_owned())),
        (Wire::EventsDown, Some("/api/events".to_owned())),
        (Wire::LogUp, None),
        (Wire::LogDown, None),
        (Wire::TunnelUp, Some("/api/agents/tunnel".to_owned())),
        (Wire::TunnelDown, Some("/api/agents/tunnel".to_owned())),
    ];
    for (w, path) in &sockets {
        let Some(path) = path else {
            for r in rows(*w) {
                say(&format!("{} {}: {} → {}; not sent here (it needs a stream or a job of its own): step 6's tests/hostile_sockets.rs sends it", w.name(), r.case.name(), r.sent, r.told));
                ran.passed.entry(*w).or_default().push(r.case);
            }
            continue;
        };
        for r in rows(*w) {
            let mut sock = agents::socket(s.port, &s.admin, path)?;
            sock.set_read_timeout(Some(Duration::from_secs(10)))?;
            let pages = ui_pages(*w, &real);
            let before = Before::of(&s, r.noted, &pages);
            // One past the socket's limit may find it closed before it's all sent. It goes on a
            // thread of its own, and what comes down meanwhile is read as it comes, the close among
            // it, as a page reads its socket while it sends: a socket that sends a lot (the
            // session's) would otherwise fill this side's buffer, and its server, closing it with
            // the rest of the frame unread, resets the connection and drops what it hadn't sent.
            let frame = ws_frame(1, &ui_frame(*w, r.case))?;
            if r.case == Case::TooLarge {
                let mut writer = sock.try_clone()?;
                writer.set_write_timeout(Some(Duration::from_secs(30)))?;
                let sending = std::thread::spawn(move || {
                    let _ = writer.write_all(&frame);
                });
                typed_answer(&mut sock, *w, &r)?;
                let _ = sending.join();
            } else {
                sock.write_all(&frame).map_err(|e| anyhow::anyhow!("{} {}: {e}", w.name(), r.case.name()))?;
                typed_answer(&mut sock, *w, &r)?;
            }
            before.noted_once(&s, r.noted, &pages)?;
            // The socket goes on (a new one, after one too large), and the next valid message on it
            // is applied: on the session's socket, a command answered; on the rest, a ping.
            if r.case == Case::TooLarge {
                sock = agents::socket(s.port, &s.admin, path)?;
                sock.set_read_timeout(Some(Duration::from_secs(10)))?;
            }
            if *w == Wire::SessionUp || *w == Wire::SessionDown {
                let id = format!("hostile-next-{}-{}", w.name(), r.case.name().replace(' ', "-"));
                sock.write_all(&ws_frame(1, json!({"t": "cmd", "id": id, "c": {"type": "set_model", "model": id}}).to_string().as_bytes())?)?;
                let deadline = Instant::now() + Duration::from_secs(60);
                loop {
                    let (op, payload) = agents::frame(&mut sock)?;
                    if op == 1 && serde_json::from_slice::<Value>(&payload).ok().is_some_and(|f| f.get("t").and_then(Value::as_str) == Some("ack") && f.get("id").and_then(Value::as_str) == Some(id.as_str())) {
                        break;
                    }
                    anyhow::ensure!(Instant::now() < deadline, "{} {}: the next command was never answered", w.name(), r.case.name());
                }
            } else {
                sock.write_all(&ws_frame(9, b"next")?)?;
                let deadline = Instant::now() + Duration::from_secs(30);
                loop {
                    match agents::frame(&mut sock) {
                        Ok((10, payload)) if payload == b"next" => break,
                        Ok(_) => {}
                        Err(e) => anyhow::bail!("{} {}: the socket didn't go on: {e:#}", w.name(), r.case.name()),
                    }
                    anyhow::ensure!(Instant::now() < deadline, "{} {}: no pong", w.name(), r.case.name());
                }
            }
            // The tunnel's is a runner's, known by its address; the rest are the person's.
            let (who, named) = if matches!(w, Wire::TunnelUp | Wire::TunnelDown) { ("address:127.0.0.1", "its address") } else { ("admin", "the person") };
            ran.passed(*w, &r, who, &format!("answered typed; counted, audited naming {named}, on its page"), "the socket went on, the next message applied");
        }
    }
    ran.audited(&s)?;
    ran.covers(&sockets.map(|(w, _)| w))
}
