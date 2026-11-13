//! An Agentfile checked by Shards itself, before kiln keeps it: Shards' own Agentfile parser and build
//! checks (shards-agentfile, at kiln's one Shards revision in vendor/shards; kiln plan
//! 01a1cd8e862e7169a074db3d5fa33f3d steps 16 and 30), run in kiln's own process, so every text is checked,
//! always, on every server. The check is BuildKit's lint subrequest as Shards ports it (what `shards build
//! --call=check,format=json` answers: shards' `crates/shards/src/build/mod.rs` `Call::Check`,
//! `crates/agentfile/src/plan.rs` `lint`, D70): its warnings, and the error that ended the planning, each
//! with its lines, in Shards' words.
//!
//! Planned whole, a check resolves every stage's base by pulling it into Shards' store, as a build does
//! (`Bases::fetch` calls `crate::pull::fetch`): on this server that would take each base's layers, need
//! its registry's credentials (a project's own image in kiln's registry has none here), and refuse an
//! Agentfile whose registry is away. So the check names a target no stage can be named (stage names are
//! `^[a-z][a-z0-9-_.]*$`), and Shards' planning, held to Dockerfile2LLB's order, stops where it looks
//! for it: after the text is parsed, its instructions and the Agentfile's directives read, its names and
//! grants checked (`agentfile::check`: one namespace, `CONNECT`, `ATTACH`, `EXPOSE … FOR` and `FOR`
//! scopes), its stage names and casing linted; before any base is resolved. What stops it there is
//! that target alone (`target stage "kiln:static-check" could not be found`), which says the text passed;
//! any other error is the text's. Measured on shards 0.7.0 (origin/main b8a47407) as its binary, with no
//! network (`unshare -rn`): every case below in 3 to 42 ms, its home left empty; the same check planned
//! whole failed resolving `ubuntu:24.04` ("Network is unreachable"). Here no base can be resolved at all
//! ([`Unresolved`]): a check that reached one would say so in its refusal. Warnings Shards finds only as
//! it dispatches steps (`UndefinedVar` ...) are the build's to say.
//!
//! kiln adds one rule of its own, on a text Shards passed: no grant may reach kiln itself
//! ([`Kiln`]), as kiln's MCP rides the agent-control link's own `mcp` lane (Shards D254) and never
//! the network (ROOT's decision, 2026-11-13; kiln plan 01a1cd8e862e7169a074db3d5fa33f3d step 18).

use std::net::IpAddr;

use serde::Serialize;
use shards_agentfile::{lint as rules, parser, plan, platform::Platform};

/// The target no stage is named: Shards' plan stops where it looks for it, its static checks done.
const TARGET: &str = "kiln:static-check";

/// What Shards said of an Agentfile's text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentfileCheck"))]
pub struct Check {
    /// Whether Shards read it; `unchecked` says why not.
    pub checked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub unchecked: Option<Unchecked>,
    /// What it found: its warnings, and the error that refuses the text, if any, in Shards' words.
    pub diagnostics: Vec<Diagnostic>,
}

/// Why Shards didn't check it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentfileUnchecked"))]
pub enum Unchecked {
    /// No `shards` on the server's `PATH`: an older kiln's, which asked one; this kiln checks every text in its
    /// own process.
    NoShards,
    /// It didn't answer in time: an older kiln's.
    TimedOut,
    /// It answered no check: its exit status (none when it didn't start) and its own words; here, the
    /// check's own end (the pool it ran on gone) in kiln's words.
    Failed { status: Option<i32>, said: String },
}

/// One thing Shards found, where it is, as the editor's diagnostics are (`ci.images.validate`'s).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentfileDiagnostic"))]
pub struct Diagnostic {
    /// The check's rule (`StageNameCasing`), or `shards` for the error that ended its planning.
    pub code: String,
    pub severity: Severity,
    pub message: String,
    /// The file it's in: `Agentfile`.
    pub path: String,
    pub range: Range,
    /// The rule's documentation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentfileSeverity"))]
pub enum Severity {
    /// Shards refuses the text.
    Error,
    Warning,
}

/// Whole lines, from 1: Shards' ranges name lines alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentfileRange"))]
pub struct Range {
    pub start: Pos,
    pub end: Pos,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentfilePos"))]
pub struct Pos {
    pub line: u32,
    pub col: u32,
}

impl Check {
    fn unchecked(why: Unchecked) -> Check {
        Check { checked: false, unchecked: Some(why), diagnostics: Vec::new() }
    }

    /// The error that refuses the text, in Shards' words, with the line it's at when its words don't
    /// say it.
    pub fn refusal(&self) -> Option<String> {
        let d = self.diagnostics.iter().find(|d| d.severity == Severity::Error)?;
        let line = d.range.start.line;
        Some(if d.message.contains(&format!("line {line}")) { d.message.clone() } else { format!("line {line}: {}", d.message) })
    }
}

/// `text` checked by Shards, then by kiln's own rule ([`reaching_kiln`]), on the compute pool (its
/// parse and plan are CPU work): unchecked only if the check itself never ended there.
pub async fn check(text: &str, kiln: &Kiln) -> Check {
    let (text, kiln) = (text.to_owned(), kiln.clone());
    match crate::rt::compute(move || with_kiln(lint(&text), &text, &kiln)).await {
        Ok(c) => c,
        Err(e) => Check::unchecked(Unchecked::Failed { status: None, said: format!("{e:#}") }),
    }
}

/// `text` checked by Shards on this thread, as `shards build --call=check,format=json --target
/// kiln:static-check -f Agentfile` checks it: the Agentfile dialect, this host's platform as its build's
/// and its target's (shards' `build::host_platform`), no build arguments, labels or ignore file, bases
/// resolved as `shards build` resolves them by default (`image-resolve-mode` local).
pub fn lint(text: &str) -> Check {
    let host = Platform::new(
        "linux",
        match std::env::consts::ARCH {
            "aarch64" => "arm64",
            "x86_64" => "amd64",
            other => other,
        },
    );
    let opts = plan::Options {
        target_platform: host.clone(),
        build_platforms: vec![host],
        implicit_target: true,
        target: TARGET.as_bytes().to_vec(),
        dialect: parser::Dialect::Agentfile,
        ignore_name: b".agentignore".to_vec(),
        image_resolve_mode: b"local".to_vec(),
        ..plan::Options::default()
    };
    let mut diagnostics = Vec::new();
    match plan::lint(text.as_bytes(), &opts, &Unresolved) {
        Ok(found) => {
            diagnostics.extend(found.warnings.iter().map(warning));
            let passed = format!("target stage {TARGET:?} could not be found");
            if let Some((message, location)) = found.error.filter(|(m, _)| !m.starts_with(passed.as_bytes())) {
                diagnostics.push(refused(String::from_utf8_lossy(&message).into_owned(), &location));
            }
        }
        // What fails the request itself, before its checks are asked (a `# syntax=` line naming a frontend
        // Shards doesn't run): as `shards build` ends with it (`failed to build: failed to solve: …`).
        Err(e) => {
            diagnostics.extend(e.warnings.iter().map(warning));
            let location: Vec<(usize, usize)> = e.location.concat();
            diagnostics.push(refused(format!("failed to build: failed to solve: {}", String::from_utf8_lossy(&e.message)), &location));
        }
    }
    diagnostics.sort_by_key(|d| (d.range.start.line, d.severity == Severity::Warning));
    Check { checked: true, unchecked: None, diagnostics }
}

/// Shards' check `c` of `text`, with each grant that reaches kiln refused at its line where Shards
/// refused nothing.
fn with_kiln(mut c: Check, text: &str, kiln: &Kiln) -> Check {
    if c.checked && c.refusal().is_none() {
        c.diagnostics.extend(reaching_kiln(text, kiln));
        c.diagnostics.sort_by_key(|d| (d.range.start.line, d.severity == Severity::Warning));
    }
    c
}

/// The code of kiln's refusal of a grant that reaches kiln itself.
pub const REACHES_KILN: &str = "ReachesKiln";

/// The machine itself, as a microVM names it (Shards D236's name, Docker Desktop's: `.internal` is
/// the top-level domain ICANN reserved for private use, so no name in the DNS is it).
const HOST: &str = "host.docker.internal";

/// What is kiln itself, which no Agentfile may grant a way to (ROOT, 2026-11-13): kiln's MCP rides
/// the agent-control link's `mcp` lane (Shards D254), so a grant to kiln is a way around that lane.
/// Third-party servers stay grantable by name, as default deny has them. kiln is:
/// - the machine its runner is: `host.docker.internal` (D236), and, as literals, its loopback,
///   link-local and unspecified addresses (D124's host network) and a network's declared gateway
///   (D31's `host-gateway`);
/// - kiln's own server: the hosts its configured URLs, advertised addresses and certificate names
///   give, a name by its name and an address by its address.
///
/// A name that only resolves to one of these, or a literal address of the runner's own interfaces,
/// is for the run to see as it starts on that machine (owed with step 17's launch): this check
/// resolves no name, as it pulls no base.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Kiln {
    /// kiln's server's names, lowercase.
    names: Vec<String>,
    /// kiln's server's addresses.
    addrs: Vec<IpAddr>,
}

/// Which part of kiln a grant reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Itself {
    /// The machine the runner is.
    Machine,
    /// kiln's server.
    Server,
}

impl Kiln {
    /// kiln's server as `server` configures it: its public and pages URLs' hosts, its advertised
    /// QUIC and HTTP/3 addresses, the addresses it listens on where they name one, and its
    /// certificate's extra names.
    pub fn of(server: &crate::config::Server) -> Kiln {
        let mut k = Kiln::default();
        for url in [Some(server.public_url.as_str()), server.pages_url.as_deref()].into_iter().flatten() {
            k.add(url_host(url));
        }
        for at in [Some(server.quic_advertise.as_str()), server.h3_advertise.as_deref(), Some(server.http_addr.as_str()), Some(server.quic_addr.as_str()), server.h3_addr.as_deref()].into_iter().flatten() {
            k.add(authority_host(at));
        }
        for name in &server.tls_names {
            k.add(name);
        }
        k
    }

    /// One of kiln's hosts: an address (an unspecified one listens everywhere, naming none) or a name.
    fn add(&mut self, host: &str) {
        let host = host.trim().trim_start_matches('[').trim_end_matches(']').trim_end_matches('.');
        match host.parse::<IpAddr>() {
            Ok(ip) if ip.is_unspecified() => {}
            Ok(ip) => {
                if !self.addrs.contains(&ip) {
                    self.addrs.push(ip);
                }
            }
            Err(_) if host.is_empty() => {}
            Err(_) => {
                let name = host.to_ascii_lowercase();
                if !self.names.contains(&name) {
                    self.names.push(name);
                }
            }
        }
    }

    /// Whether `host`, a grant's, is kiln itself, given the gateways its Agentfile declares.
    fn itself(&self, host: &str, gateways: &[IpAddr]) -> Option<Itself> {
        let host = host.trim_start_matches('[').trim_end_matches(']').trim_end_matches('.').to_ascii_lowercase();
        if host == HOST {
            return Some(Itself::Machine);
        }
        match host.parse::<IpAddr>() {
            Ok(ip) if ip.is_loopback() || ip.is_unspecified() || link_local(ip) || gateways.contains(&ip) => Some(Itself::Machine),
            Ok(ip) => self.addrs.contains(&ip).then_some(Itself::Server),
            Err(_) => self.names.contains(&host).then_some(Itself::Server),
        }
    }
}

/// Whether `ip` is link-local (RFC 3927's 169.254.0.0/16, RFC 4291's fe80::/10).
fn link_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_unicast_link_local(),
    }
}

/// The host a URL names: its authority's, without user or port.
fn url_host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    authority_host(rest.split(['/', '?', '#']).next().unwrap_or_default())
}

/// The host of `host:port`, `[v6]:port` or a bare host, without user or port.
fn authority_host(at: &str) -> &str {
    let at = at.rsplit('@').next().unwrap_or_default();
    if let Some(v6) = at.strip_prefix('[') {
        return v6.split(']').next().unwrap_or_default();
    }
    match at.rsplit_once(':') {
        // One colon: a port after a name or an IPv4 address; more: a bare IPv6 address.
        Some((host, port)) if !host.contains(':') && port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => at,
    }
}

/// Each grant of `text` that reaches kiln itself, refused at its line: a network's destination
/// (`NETWORK --egress=HOST:PORT`, D236) or a remote MCP server (`MCP <name> FROM <url>`), in any
/// stage. Read with Shards' own parser, as `lint` passed it.
fn reaching_kiln(text: &str, kiln: &Kiln) -> Vec<Diagnostic> {
    use shards_agentfile::agentfile::{self, Directive, Direction};
    use shards_agentfile::instructions::{self, Kind};
    let Ok(parsed) = parser::parse_as(text.as_bytes(), parser::Dialect::Agentfile) else { return Vec::new() };
    let Ok(read) = instructions::parse(&parsed, &rules::Linter::default()) else { return Vec::new() };
    let commands: Vec<&instructions::Command> = read.stages.iter().flat_map(|s| &s.commands).collect();
    let gateways: Vec<IpAddr> = commands
        .iter()
        .filter_map(|c| match &c.kind {
            Kind::Agentfile(Directive::Network(n)) => Some(&n.gateways),
            _ => None,
        })
        .flatten()
        .filter_map(|g| std::str::from_utf8(g).ok()?.parse().ok())
        .collect();
    let mut out = Vec::new();
    for c in commands {
        let hosts: Vec<(Vec<u8>, u16)> = match &c.kind {
            Kind::Agentfile(Directive::Network(n)) => n.ports.iter().filter(|(_, d)| *d == Direction::Egress).filter_map(|(p, _)| agentfile::destination_of(p)?.ok()).collect(),
            Kind::Agentfile(Directive::Mcp(m)) => agentfile::mcp_endpoint(&m.source).into_iter().collect(),
            _ => continue,
        };
        for (host, port) in hosts {
            let host = String::from_utf8_lossy(&host).into_owned();
            let Some(what) = kiln.itself(&host, &gateways) else { continue };
            let what = match what {
                Itself::Machine => "the machine kiln's runner is",
                Itself::Server => "kiln's own server",
            };
            out.push(Diagnostic {
                code: REACHES_KILN.into(),
                severity: Severity::Error,
                message: format!("{host}:{port} is {what}, which no Agentfile may grant: kiln's MCP rides the agent-control lane (D254)"),
                path: "Agentfile".into(),
                range: range(&c.location),
                url: None,
            });
        }
    }
    out
}

/// A check's warning, as the lint subrequest's JSON says it: its detail, else its rule's description.
fn warning(w: &rules::Warning) -> Diagnostic {
    let detail = String::from_utf8_lossy(&w.message).into_owned();
    Diagnostic {
        message: if detail.is_empty() { w.description.to_owned() } else { detail },
        code: w.rule.to_owned(),
        severity: Severity::Warning,
        path: "Agentfile".into(),
        range: range(&w.location),
        url: Some(w.url.to_owned()).filter(|u| !u.is_empty()),
    }
}

/// The error that refuses the text.
fn refused(message: String, location: &[(usize, usize)]) -> Diagnostic {
    Diagnostic { code: "shards".into(), severity: Severity::Error, message, path: "Agentfile".into(), range: range(location), url: None }
}

/// A location's lines, first to last (line 1 where it names none).
fn range(location: &[(usize, usize)]) -> Range {
    let line = |l: usize| u32::try_from(l).unwrap_or(u32::MAX);
    let first = location.iter().map(|r| r.0).filter(|l| *l > 0).min().map_or(1, line);
    let last = location.iter().map(|r| line(r.1.max(r.0))).max().unwrap_or(first).max(first);
    Range { start: Pos { line: first, col: 1 }, end: Pos { line: last, col: 1 } }
}

/// Shards' store, standing in: no base is resolved by a static check (its planning stops at
/// [`TARGET`] before it asks for one), and one asked for is refused, saying why.
struct Unresolved;

impl plan::Resolver for Unresolved {
    fn resolve(&self, name: &[u8], _: Option<&Platform>, _: &[u8]) -> Result<plan::Resolved, Vec<u8>> {
        Err([name, b": kiln checks an Agentfile without resolving its bases"].concat())
    }

    fn epoch(&self, _: &plan::EpochSource) -> Result<Option<plan::Epoch>, Vec<u8>> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shards' own words, as its binary said them for these texts (shards 0.7.0, origin/main b8a47407,
    // `test-release`, `build --call=check,format=json --progress=quiet --target kiln:static-check`, run
    // with no network, its home empty): the in-process check is held to them word for word.

    /// Two agents joined on a network: nothing to say.
    const PASSED: &str = "FROM ubuntu:24.04\nAGENT main FROM ./agent\nAGENT helper FROM ./helper\nNETWORK team FOR main helper\nCONNECT --port=8080 main TO helper ON team\nCMD echo hi\n";

    /// A CONNECT naming no member: refused at its line.
    const NAMES: &str = "FROM ubuntu:24.04\nAGENT main FROM ./agent\nNETWORK team FOR main\nCONNECT --port=8080 main TO nobody ON team\n";
    const NAMES_SAID: &str = "dockerfile parse error on line 4: CONNECT names \"nobody\", which no AGENT or HARNESS of this stage's lineage declares";

    /// A stage named in capitals, and a lowercase instruction among uppercase ones: two warnings.
    const WARNED: &str = "from ubuntu:24.04 as Base\nFROM base\nCMD echo hi\n";

    /// A `# syntax=` line naming a frontend Shards doesn't run: the request fails, before any check.
    const FRONTEND: &str = "# syntax=example.com/custom/frontend:1\nFROM ubuntu:24.04\n\n";
    const FRONTEND_SAID: &str = "failed to build: failed to solve: shards cannot run frontend example.com/custom/frontend:1: it builds Dockerfiles with its own port of docker/dockerfile 1.27.1";

    #[test]
    fn step16_the_target_that_stops_shards_says_the_text_passed() {
        let c = lint(PASSED);
        assert_eq!(c, Check { checked: true, unchecked: None, diagnostics: Vec::new() });
        assert_eq!(c.refusal(), None);
    }

    #[test]
    fn step16_any_other_error_refuses_it_in_shards_words_at_its_line() -> anyhow::Result<()> {
        let c = lint(NAMES);
        let [d] = c.diagnostics.as_slice() else { anyhow::bail!("{c:?}") };
        anyhow::ensure!(c.checked && (d.code.as_str(), d.severity, d.range.start.line, d.range.end.line) == ("shards", Severity::Error, 4, 4), "{c:?}");
        assert_eq!(c.refusal().as_deref(), Some(NAMES_SAID));
        Ok(())
    }

    #[test]
    fn step16_warnings_are_diagnostics_that_refuse_nothing() {
        let c = lint(WARNED);
        let codes: Vec<(&str, Severity, u32)> = c.diagnostics.iter().map(|d| (d.code.as_str(), d.severity, d.range.start.line)).collect();
        assert_eq!(codes, [("StageNameCasing", Severity::Warning, 1), ("ConsistentInstructionCasing", Severity::Warning, 1)]);
        assert_eq!(c.diagnostics.first().map(|d| d.message.as_str()), Some("Stage name 'Base' should be lowercase"));
        assert_eq!(c.diagnostics.first().and_then(|d| d.url.as_deref()), Some("https://docs.docker.com/go/dockerfile/rule/stage-name-casing/"));
        assert_eq!(c.diagnostics.get(1).map(|d| d.message.as_str()), Some("Command 'from' should match the case of the command majority (uppercase)"));
        assert_eq!(c.refusal(), None);
    }

    /// Where the shards binary answered no check (its request failed), the text is refused in the
    /// words it ended with: Shards builds no Agentfile it can't plan.
    #[test]
    fn step16_a_frontend_shards_does_not_run_refuses_it_in_shards_words() -> anyhow::Result<()> {
        let c = lint(FRONTEND);
        let [d] = c.diagnostics.as_slice() else { anyhow::bail!("{c:?}") };
        anyhow::ensure!(c.checked && (d.code.as_str(), d.severity, d.range.start.line) == ("shards", Severity::Error, 1), "{c:?}");
        assert_eq!(c.refusal().as_deref(), Some(format!("line 1: {FRONTEND_SAID}").as_str()));
        Ok(())
    }

    /// Every text is checked, on any server: an instruction Shards doesn't know is refused at its line,
    /// and no base is resolved (none can be here) for a text that names one Shards would have to pull.
    #[test]
    fn step16_every_text_is_checked_in_process_without_resolving_bases() -> anyhow::Result<()> {
        let c = crate::rt::block_on(async { Ok(check("FROM ubuntu:24.04\nFORM ubuntu\n", &Kiln::default()).await) })?;
        anyhow::ensure!(c.checked && c.refusal().is_some_and(|r| r.starts_with("dockerfile parse error on line 2: unknown instruction: FORM")), "{c:?}");
        let c = lint("FROM registry.example.invalid/nobody/base:1\nAGENT main FROM ./agent\nCMD echo hi\n");
        anyhow::ensure!(c.checked && c.diagnostics.is_empty(), "{c:?}");
        Ok(())
    }

    #[test]
    fn step16_a_refusal_without_a_line_in_its_words_names_it() {
        let c = Check {
            checked: true,
            unchecked: None,
            diagnostics: vec![Diagnostic { code: "shards".into(), severity: Severity::Error, message: "dockerfile contains no stages to build".into(), path: "Agentfile".into(), range: range(&[]), url: None }],
        };
        assert_eq!(c.refusal().as_deref(), Some("line 1: dockerfile contains no stages to build"));
    }

    #[test]
    fn step16_an_empty_text_is_refused_as_shards_refuses_it() {
        let c = lint("");
        assert_eq!(c.refusal().as_deref(), Some("line 1: the Dockerfile cannot be empty"));
    }

    /// A kiln whose server is `kiln.example.test`, and 10.1.2.3 among its certificate's names.
    fn kiln() -> Kiln {
        let mut server = crate::config::Server::default();
        server.public_url = "https://kiln.example.test/".into();
        server.tls_names = vec!["10.1.2.3".into()];
        Kiln::of(&server)
    }

    /// An agent's network out to `grant`, its boundary open on 443 and 7070, and a remote MCP server
    /// from `mcp` where one is given (line 6): a text Shards passes.
    fn granting(grant: &str, mcp: Option<&str>) -> String {
        let mut text = format!("FROM ubuntu:24.04\nAGENT main FROM ./agent\nNETWORK --egress={grant} out FOR main\nEXPOSE --agents=main 443/tcp 7070/tcp AS egress FOR out\nCONNECT main WITH main ON out\n");
        if let Some(url) = mcp {
            text.push_str(&format!("MCP tools FROM {url} FOR main\n"));
        }
        text
    }

    /// kiln's own rule (ROOT, 2026-11-13): a destination or a remote MCP server that is kiln itself (the
    /// machine its runner is, or kiln's server) is refused at its line, typed, saying kiln's MCP rides
    /// the agent-control lane; Shards passed each text.
    #[test]
    fn step18_a_grant_that_reaches_kiln_is_refused_at_its_line() -> anyhow::Result<()> {
        let kiln = kiln();
        let cases = [
            (granting("host.docker.internal:7070", None), 3),
            (granting("127.0.0.1:7070", None), 3),
            (granting("169.254.1.1:7070", None), 3),
            (granting("kiln.example.test:443", None), 3),
            (granting("10.1.2.3:7070", None), 3),
            (granting("api.github.com:443", Some("http://host.docker.internal:7070/api/mcp")), 6),
            (granting("api.github.com:443", Some("https://KILN.example.test/api/mcp")), 6),
        ];
        for (text, line) in cases {
            let shards = lint(&text);
            anyhow::ensure!(shards.refusal().is_none(), "Shards refused {text:?}: {shards:?}");
            let c = with_kiln(shards, &text, &kiln);
            let ours: Vec<&Diagnostic> = c.diagnostics.iter().filter(|d| d.code == REACHES_KILN).collect();
            let [d] = ours.as_slice() else { anyhow::bail!("{text:?}: {c:?}") };
            anyhow::ensure!(d.severity == Severity::Error && (d.range.start.line, d.range.end.line) == (line, line), "{text:?}: {d:?}");
            anyhow::ensure!(d.message.ends_with("kiln's MCP rides the agent-control lane (D254)"), "{d:?}");
            anyhow::ensure!(c.refusal().is_some_and(|r| r.starts_with(&format!("line {line}: "))), "{c:?}");
        }
        Ok(())
    }

    /// A third party's remote MCP server, and destinations that aren't kiln, stay grantable: default deny
    /// already names them, and refusing them would break what people run today.
    #[test]
    fn step18_a_third_partys_mcp_server_still_passes() {
        let text = granting("api.github.com:443", Some("https://api.githubcopilot.com/mcp/"));
        let c = with_kiln(lint(&text), &text, &kiln());
        assert!(c.checked && c.refusal().is_none() && c.diagnostics.iter().all(|d| d.code != REACHES_KILN), "{c:?}");
        let text = granting("10.1.2.4:7070", None);
        assert!(with_kiln(lint(&text), &text, &kiln()).refusal().is_none());
    }

    /// kiln's server is every host its configuration names, a name by its name in any case and an
    /// address by its address; an address it listens on everywhere names none; the machine's own
    /// names and addresses are kiln whatever the configuration.
    #[test]
    fn step18_kiln_is_what_its_configuration_names() -> anyhow::Result<()> {
        let mut server = crate::config::Server::default();
        server.public_url = "https://user@Kiln.Example.test:8443/x".into();
        server.pages_url = Some("https://pages.example.test".into());
        server.quic_advertise = "[2001:db8::7]:7071".into();
        server.http_addr = "0.0.0.0:7070".into();
        server.quic_addr = "192.0.2.10:7071".into();
        server.tls_names = vec!["kiln.lan".into(), "198.51.100.4".into()];
        let k = Kiln::of(&server);
        for host in ["kiln.example.test", "KILN.EXAMPLE.TEST", "pages.example.test", "kiln.lan", "2001:db8::7", "[2001:db8::7]", "192.0.2.10", "198.51.100.4"] {
            anyhow::ensure!(k.itself(host, &[]) == Some(Itself::Server), "{host}: {k:?}");
        }
        for host in ["host.docker.internal", "127.0.0.2", "::1", "0.0.0.0", "169.254.0.9", "fe80::1"] {
            anyhow::ensure!(Kiln::default().itself(host, &[]) == Some(Itself::Machine), "{host}");
        }
        // A network's declared gateway is the machine too (D31's `host-gateway`).
        let gateway: IpAddr = "10.9.0.1".parse()?;
        anyhow::ensure!(Kiln::default().itself("10.9.0.1", &[gateway]) == Some(Itself::Machine) && Kiln::default().itself("10.9.0.1", &[]).is_none());
        for host in ["api.github.com", "192.0.2.11", "kiln.example.test.evil"] {
            anyhow::ensure!(k.itself(host, &[]).is_none(), "{host}");
        }
        Ok(())
    }

    /// Every built-in passes kiln's rule as it passes Shards' check: none grants a way to kiln.
    #[test]
    fn step18_no_builtin_grants_a_way_to_kiln() -> anyhow::Result<()> {
        let kiln = Kiln::of(&crate::config::Server::default());
        for b in crate::agentfiles::builtin::ALL {
            let c = with_kiln(lint(b.text), b.text, &kiln);
            anyhow::ensure!(c.checked && c.refusal().is_none(), "{}: {c:?}", b.name);
        }
        Ok(())
    }
}
