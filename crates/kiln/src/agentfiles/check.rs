//! An Agentfile checked by Shards itself, before kiln keeps it: the `shards` that builds Agentfiles,
//! found on this server's `PATH` as a leg finds it on its own (`runner::runtime`'s `kiln/build-image@v1`),
//! asked for its build check. That check is BuildKit's lint subrequest as Shards ports it (`shards
//! build --call=check,format=json`, as `docker build --check`: shards' `crates/shards/src/build/mod.rs`
//! `call_of` and `Call::Check`, `crates/agentfile/src/plan.rs` `lint`, D70), which answers BuildKit's
//! `LintResults` JSON: its warnings, and the error that ended the planning, each with its lines.
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
//! any other error is the text's. Measured on shards 0.7.0 (origin/main b8a47407) with no network
//! (`unshare -rn`): every case below in 3 to 42 ms, its home left empty; the same check planned whole
//! failed resolving `ubuntu:24.04` ("Network is unreachable"). Warnings Shards finds only as it
//! dispatches steps (`UndefinedVar` ...) are the build's to say.

use std::path::Path;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::hub::Hub;

/// The target no stage is named: Shards' plan stops where it looks for it, its static checks done.
const TARGET: &str = "kiln:static-check";

/// How long a check may take: as long as kiln waits for a tool asked its version
/// ([`crate::proc::VERSION_WITHIN`]), for a check that reads no network and plans no step.
pub const WITHIN: Duration = crate::proc::VERSION_WITHIN;

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
    /// No `shards` on this server's `PATH`.
    NoShards,
    /// It didn't answer within [`WITHIN`].
    TimedOut,
    /// It answered no check: its exit status (none when it didn't start) and its own words.
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

/// `text` checked by the `shards` on this server's `PATH`: unchecked, saying why, when there is none
/// or it answers no check.
pub async fn check(hub: &Hub, text: &str) -> Check {
    match crate::os::which("shards") {
        Some(shards) => run(&shards, &hub.cfg.server.data_dir.join("agentfiles"), text).await,
        None => Check::unchecked(Unchecked::NoShards),
    }
}

/// `text` checked by `shards`, written for it to `scratch/<id>/Agentfile` (that folder its context,
/// removed after).
pub async fn run(shards: &Path, scratch: &Path, text: &str) -> Check {
    let dir = scratch.join(crate::util::new_id());
    let (d, t) = (dir.clone(), text.to_owned());
    let written = crate::rt::blocking(move || std::fs::create_dir_all(&d).and_then(|()| std::fs::write(d.join("Agentfile"), t))).await;
    if let Some(e) = match written {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(e.to_string()),
        Err(e) => Some(format!("{e:#}")),
    } {
        return Check::unchecked(Unchecked::Failed { status: None, said: format!("kiln couldn't write the Agentfile for Shards to read: {e}") });
    }
    let mut c = std::process::Command::new(shards);
    c.args(["build", "--call=check,format=json", "--progress=quiet", "--target", TARGET, "-f"]).arg(dir.join("Agentfile")).arg(&dir).current_dir(&dir);
    let started = Instant::now();
    let out = crate::proc::output(c, WITHIN).await;
    let gone = dir.clone();
    if let Ok(Err(e)) = crate::rt::blocking(move || std::fs::remove_dir_all(gone)).await {
        tracing::warn!(dir = %dir.display(), error = %e, "an Agentfile written for Shards to check stays");
    }
    match out {
        Ok(o) => read(&o.stdout, &o.stderr, o.status.code()),
        Err(_) if started.elapsed() >= WITHIN => Check::unchecked(Unchecked::TimedOut),
        Err(e) => Check::unchecked(Unchecked::Failed { status: None, said: format!("{e:#}") }),
    }
}

/// BuildKit's `LintResults`, as `--call=check,format=json` writes it (shards' `subrequests::LintResults`):
/// `sources` is in every one, so JSON without it is no check's.
#[derive(Deserialize)]
struct Lint {
    #[serde(default)]
    warnings: Option<Vec<Warning>>,
    #[serde(rename = "sources")]
    _sources: Vec<serde_json::Value>,
    #[serde(default, rename = "buildError")]
    build_error: Option<BuildError>,
}

#[derive(Deserialize)]
struct Warning {
    #[serde(rename = "ruleName")]
    rule: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    detail: String,
    #[serde(default)]
    location: Location,
}

#[derive(Deserialize)]
struct BuildError {
    message: String,
    #[serde(default)]
    location: Location,
}

#[derive(Deserialize, Default)]
struct Location {
    #[serde(default)]
    ranges: Vec<LineRange>,
}

#[derive(Deserialize)]
struct LineRange {
    #[serde(default)]
    start: Line,
    #[serde(default)]
    end: Line,
}

#[derive(Deserialize, Default)]
struct Line {
    #[serde(default)]
    line: u32,
}

impl Location {
    /// Its lines, first to last (line 1 where it names none).
    fn range(&self) -> Range {
        let first = self.ranges.iter().map(|r| r.start.line).filter(|l| *l > 0).min().unwrap_or(1);
        let last = self.ranges.iter().map(|r| r.end.line.max(r.start.line)).max().unwrap_or(first).max(first);
        Range { start: Pos { line: first, col: 1 }, end: Pos { line: last, col: 1 } }
    }
}

/// What `shards build --call=check,format=json --target kiln:static-check` answered: its result, else
/// unchecked with its last `ERROR:` line (`failed`'s, as `shards build` ends), or the last it said.
pub fn read(stdout: &[u8], stderr: &[u8], status: Option<i32>) -> Check {
    let Ok(lint) = serde_json::from_slice::<Lint>(stdout) else {
        let said = String::from_utf8_lossy(stderr);
        let line = said.lines().rev().find_map(|l| l.strip_prefix("ERROR: ")).or_else(|| said.lines().rev().map(str::trim).find(|l| !l.is_empty()));
        return Check::unchecked(Unchecked::Failed { status, said: line.unwrap_or("nothing").to_owned() });
    };
    let mut diagnostics: Vec<Diagnostic> = lint
        .warnings
        .unwrap_or_default()
        .into_iter()
        .map(|w| Diagnostic {
            message: if w.detail.is_empty() { w.description } else { w.detail },
            code: w.rule,
            severity: Severity::Warning,
            path: "Agentfile".into(),
            range: w.location.range(),
            url: Some(w.url).filter(|u| !u.is_empty()),
        })
        .collect();
    let passed = format!("target stage {TARGET:?} could not be found");
    if let Some(e) = lint.build_error.filter(|e| !e.message.starts_with(&passed)) {
        diagnostics.push(Diagnostic { code: "shards".into(), severity: Severity::Error, message: e.message, path: "Agentfile".into(), range: e.location.range(), url: None });
    }
    diagnostics.sort_by_key(|d| (d.range.start.line, d.severity == Severity::Warning));
    Check { checked: true, unchecked: None, diagnostics }
}

#[cfg(test)]
mod tests {
    use super::*;

    // What shards 0.7.0 (origin/main b8a47407, `test-release`) wrote for `--call=check,format=json
    // --progress=quiet --target kiln:static-check`, run with no network (`unshare -rn`), its home empty.

    const PASSED: &str = r#"{
  "warnings": null,
  "sources": [
    {
      "filename": "Agentfile",
      "data": "RlJPTSB1YnVudHU6MjQuMDQKQUdFTlQgbWFpbiBGUk9NIC4vYWdlbnQKQUdFTlQgaGVscGVyIEZST00gLi9oZWxwZXIKTkVUV09SSyB0ZWFtIEZPUiBtYWluIGhlbHBlcgpDT05ORUNUIC0tcG9ydD04MDgwIG1haW4gVE8gaGVscGVyIE9OIHRlYW0KQ01EIGVjaG8gaGkK",
      "language": "Agentfile"
    }
  ],
  "buildError": {
    "message": "target stage \"kiln:static-check\" could not be found",
    "location": {}
  }
}
"#;

    const NAMES: &str = r#"{
  "warnings": null,
  "sources": [
    {
      "filename": "Agentfile",
      "data": "RlJPTSB1YnVudHU6MjQuMDQKQUdFTlQgbWFpbiBGUk9NIC4vYWdlbnQKTkVUV09SSyB0ZWFtIEZPUiBtYWluCkNPTk5FQ1QgLS1wb3J0PTgwODAgbWFpbiBUTyBub2JvZHkgT04gdGVhbQo=",
      "language": "Agentfile"
    }
  ],
  "buildError": {
    "message": "dockerfile parse error on line 4: CONNECT names \"nobody\", which no AGENT or HARNESS of this stage's lineage declares",
    "location": {
      "ranges": [
        {
          "start": {
            "line": 4
          },
          "end": {
            "line": 4
          }
        }
      ]
    }
  }
}
"#;

    const WARNED: &str = r#"{
  "warnings": [
    {
      "ruleName": "StageNameCasing",
      "description": "Stage names should be lowercase",
      "url": "https://docs.docker.com/go/dockerfile/rule/stage-name-casing/",
      "detail": "Stage name 'Base' should be lowercase",
      "location": {
        "ranges": [
          {
            "start": {
              "line": 1
            },
            "end": {
              "line": 1
            }
          }
        ]
      }
    },
    {
      "ruleName": "ConsistentInstructionCasing",
      "description": "All commands within the Dockerfile should use the same casing (either upper or lower)",
      "url": "https://docs.docker.com/go/dockerfile/rule/consistent-instruction-casing/",
      "detail": "Command 'from' should match the case of the command majority (uppercase)",
      "location": {
        "ranges": [
          {
            "start": {
              "line": 1
            },
            "end": {
              "line": 1
            }
          }
        ]
      }
    }
  ],
  "sources": [
    {
      "filename": "Agentfile",
      "data": "ZnJvbSB1YnVudHU6MjQuMDQgYXMgQmFzZQpGUk9NIGJhc2UKQ01EIGVjaG8gaGkK",
      "language": "Agentfile"
    }
  ],
  "buildError": {
    "message": "target stage \"kiln:static-check\" could not be found",
    "location": {}
  }
}
"#;

    /// Its stderr for a `# syntax=` line naming a frontend it doesn't run: no check's result came.
    const FRONTEND: &str = "Agentfile:1\n--------------------\n   1 | >>> # syntax=example.com/custom/frontend:1\n   2 |     FROM ubuntu:24.04\n   3 |     \n--------------------\nERROR: failed to build: failed to solve: shards cannot run frontend example.com/custom/frontend:1: it builds Dockerfiles with its own port of docker/dockerfile 1.27.1\n";

    #[test]
    fn step30_the_target_that_stops_shards_says_the_text_passed() {
        let c = read(PASSED.as_bytes(), b"ERROR: target stage \"kiln:static-check\" could not be found\n", Some(1));
        assert_eq!(c, Check { checked: true, unchecked: None, diagnostics: Vec::new() });
        assert_eq!(c.refusal(), None);
    }

    #[test]
    fn step30_any_other_error_refuses_it_in_shards_words_at_its_line() -> anyhow::Result<()> {
        let c = read(NAMES.as_bytes(), b"", Some(1));
        let [d] = c.diagnostics.as_slice() else { anyhow::bail!("{c:?}") };
        anyhow::ensure!(c.checked && (d.code.as_str(), d.severity, d.range.start.line, d.range.end.line) == ("shards", Severity::Error, 4, 4), "{c:?}");
        assert_eq!(c.refusal().as_deref(), Some("dockerfile parse error on line 4: CONNECT names \"nobody\", which no AGENT or HARNESS of this stage's lineage declares"));
        Ok(())
    }

    #[test]
    fn step30_warnings_are_diagnostics_that_refuse_nothing() {
        let c = read(WARNED.as_bytes(), b"", Some(1));
        let codes: Vec<(&str, Severity, u32)> = c.diagnostics.iter().map(|d| (d.code.as_str(), d.severity, d.range.start.line)).collect();
        assert_eq!(codes, [("StageNameCasing", Severity::Warning, 1), ("ConsistentInstructionCasing", Severity::Warning, 1)]);
        assert_eq!(c.diagnostics.first().map(|d| d.message.as_str()), Some("Stage name 'Base' should be lowercase"));
        assert_eq!(c.diagnostics.first().and_then(|d| d.url.as_deref()), Some("https://docs.docker.com/go/dockerfile/rule/stage-name-casing/"));
        assert_eq!(c.refusal(), None);
    }

    #[test]
    fn step30_no_result_is_unchecked_saying_shards_last_error() {
        let c = read(b"", FRONTEND.as_bytes(), Some(1));
        assert_eq!(
            c.unchecked,
            Some(Unchecked::Failed { status: Some(1), said: "failed to build: failed to solve: shards cannot run frontend example.com/custom/frontend:1: it builds Dockerfiles with its own port of docker/dockerfile 1.27.1".into() })
        );
        assert!(!c.checked && c.refusal().is_none());
        // JSON that isn't a check's result is none either.
        assert!(!read(br#"{"targets": []}"#, b"", Some(0)).checked);
    }

    #[test]
    fn step30_a_refusal_without_a_line_in_its_words_names_it() {
        let c = Check {
            checked: true,
            unchecked: None,
            diagnostics: vec![Diagnostic { code: "shards".into(), severity: Severity::Error, message: "dockerfile contains no stages to build".into(), path: "Agentfile".into(), range: Location::default().range(), url: None }],
        };
        assert_eq!(c.refusal().as_deref(), Some("line 1: dockerfile contains no stages to build"));
    }

    /// A `shards` that writes what it's given to say (a stand-in, on Unix), run as the server runs one:
    /// its arguments, its folder removed after, and what it answers read.
    #[cfg(unix)]
    #[test]
    fn step30_shards_is_asked_for_its_check_in_a_folder_of_its_own() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        crate::rt::block_on(async {
            let root = tempfile::tempdir()?;
            let (bin, scratch) = (root.path().join("shards"), root.path().join("scratch"));
            let said = root.path().join("args");
            std::fs::write(&bin, format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\ncat \"$PWD/Agentfile\" > '{}.text'\nprintf '%s' '{}'\nexit 1\n", said.display(), said.display(), NAMES.replace('\'', "'\\''")))?;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))?;
            let c = run(&bin, &scratch, "FROM ubuntu:24.04\n").await;
            anyhow::ensure!(c.refusal().is_some_and(|r| r.contains("CONNECT names")), "{c:?}");
            let args = std::fs::read_to_string(&said)?;
            let args: Vec<&str> = args.lines().collect();
            anyhow::ensure!(args.get(..5) == Some(&["build", "--call=check,format=json", "--progress=quiet", "--target", "kiln:static-check"][..]), "{args:?}");
            anyhow::ensure!(args.get(5) == Some(&"-f") && args.get(6).is_some_and(|f| f.ends_with("/Agentfile")), "{args:?}");
            anyhow::ensure!(std::fs::read_to_string(format!("{}.text", said.display()))? == "FROM ubuntu:24.04\n");
            anyhow::ensure!(std::fs::read_dir(&scratch)?.next().is_none(), "the folder it was given stays");
            // One that says nothing kiln reads, and one that isn't there.
            std::fs::write(&bin, "#!/bin/sh\necho 'ERROR: no data directory: set SHARDS_HOME' >&2\nexit 1\n")?;
            let c = run(&bin, &scratch, "FROM scratch\n").await;
            anyhow::ensure!(c.unchecked == Some(Unchecked::Failed { status: Some(1), said: "no data directory: set SHARDS_HOME".into() }), "{c:?}");
            let c = run(&root.path().join("none"), &scratch, "FROM scratch\n").await;
            anyhow::ensure!(matches!(c.unchecked, Some(Unchecked::Failed { status: None, .. })), "{c:?}");
            Ok(())
        })
    }

    /// The real one, where a test names it (`KILN_TEST_SHARDS`, a `shards` binary): a valid Agentfile
    /// passes with no network asked for, and Shards' own words refuse one whose names don't hold.
    #[test]
    fn step30_a_real_shards_checks_without_resolving_bases() -> anyhow::Result<()> {
        let Some(bin) = std::env::var_os("KILN_TEST_SHARDS") else { return Ok(()) };
        crate::rt::block_on(async move {
            let root = tempfile::tempdir()?;
            let valid = "FROM ubuntu:24.04\nAGENT main FROM ./agent\nAGENT helper FROM ./helper\nNETWORK team FOR main helper\nCONNECT --port=8080 main TO helper ON team\nCMD echo hi\n";
            let c = run(Path::new(&bin), root.path(), valid).await;
            anyhow::ensure!(c.checked && c.diagnostics.is_empty(), "{c:?}");
            let c = run(Path::new(&bin), root.path(), "FROM ubuntu:24.04\nAGENT main FROM ./agent\nNETWORK team FOR main\nCONNECT --port=8080 main TO nobody ON team\n").await;
            anyhow::ensure!(c.refusal().is_some_and(|r| r.contains("line 4") && r.contains("nobody")), "{c:?}");
            let c = run(Path::new(&bin), root.path(), "FROM ubuntu:24.04\nFORM ubuntu\n").await;
            anyhow::ensure!(c.refusal().is_some_and(|r| r.contains("unknown instruction: FORM")), "{c:?}");
            Ok(())
        })
    }
}
