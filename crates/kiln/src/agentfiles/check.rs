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

/// `text` checked by Shards, on the compute pool (its parse and plan are CPU work): unchecked only if
/// the check itself never ended there.
pub async fn check(text: &str) -> Check {
    let text = text.to_owned();
    match crate::rt::compute(move || lint(&text)).await {
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
        let c = crate::rt::block_on(async { Ok(check("FROM ubuntu:24.04\nFORM ubuntu\n").await) })?;
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
}
