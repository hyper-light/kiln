//! Test reports and flaky-test classification (docs/ci.md W16).
//!
//! - [`parse`] reads a report in any of W16-R01's formats into [`Case`]s: JUnit XML in its common
//!   dialects (Surefire, pytest, jest-junit, go-junit-report, nextest), TAP, `go test -json`,
//!   libtest JSON (cargo's and nextest's), TRX, xUnit, Mocha JSON and CTRF JSON. [`detect`]
//!   tells them apart when the format isn't given. Malformed reports are errors, never panics.
//! - [`flaky`] classifies a test from its results (W16-R04).
//! - [`quarantine`] reads `.kiln/quarantine.yaml` and judges a step's failures (W16-R05).
//! - [`metrics`]: benchmark metrics from `$KILN_METRICS` and the common tools (W16-R07).

mod junit;
mod lines;
pub mod metrics;
mod objects;
pub mod quarantine;
pub mod xml;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// How a test ended.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    #[default]
    Passed,
    Failed,
    Skipped,
    /// It couldn't run, or the harness broke around it.
    Error,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Passed => "passed",
            Outcome::Failed => "failed",
            Outcome::Skipped => "skipped",
            Outcome::Error => "error",
        }
    }

    pub fn parse(s: &str) -> Option<Outcome> {
        Some(match s {
            "passed" => Outcome::Passed,
            "failed" => Outcome::Failed,
            "skipped" => Outcome::Skipped,
            "error" => Outcome::Error,
            _ => return None,
        })
    }

    /// Failed or broke.
    pub fn is_failure(self) -> bool {
        matches!(self, Outcome::Failed | Outcome::Error)
    }
}

/// One test's result in a report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Case {
    /// Its class, module, package or describe block; the report's suite otherwise.
    pub suite: String,
    pub name: String,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub outcome: Outcome,
    pub duration_ms: Option<u64>,
    pub message: Option<String>,
    /// The first [`MAX_TRACE`] bytes of its trace or output.
    pub trace: Option<String>,
    /// Runs the tool retried within the report before it passed (Surefire's and nextest's
    /// `flakyFailure`, CTRF's `retries` of a flaky test): failures that didn't stay.
    pub retried: u32,
}

/// The most of a trace kept (W16-R02).
pub const MAX_TRACE: usize = 8 << 10;

impl Case {
    /// The test's identity in a repository (W16-R02): the hash of its suite, name and file.
    pub fn id(&self) -> String {
        let mut h = Sha256::new();
        h.update(self.suite.as_bytes());
        h.update([0]);
        h.update(self.name.as_bytes());
        h.update([0]);
        h.update(self.file.as_deref().unwrap_or_default().as_bytes());
        h.finalize().iter().take(12).map(|b| format!("{b:02x}")).collect()
    }
}

/// A trace or output, cut to [`MAX_TRACE`] at a character boundary; `None` when empty.
pub(crate) fn trace(s: &str) -> Option<String> {
    let s = s.trim_end();
    if s.trim().is_empty() {
        return None;
    }
    if s.len() <= MAX_TRACE {
        return Some(s.to_owned());
    }
    let mut end = MAX_TRACE;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    Some(s.get(..end).unwrap_or_default().to_owned())
}

/// Seconds as a report writes them (`1.5`, `1,234.5`), in milliseconds.
pub(crate) fn seconds_ms(s: &str) -> Option<u64> {
    let v: f64 = s.trim().replace(',', "").parse().ok()?;
    (v.is_finite() && v >= 0.0).then(|| (v * 1000.0).round() as u64)
}

/// A report's format (W16-R01).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Format {
    Junit,
    Tap,
    GoTestJson,
    LibtestJson,
    Trx,
    Xunit,
    MochaJson,
    Ctrf,
}

impl Format {
    pub const ALL: [Format; 8] = [Format::Junit, Format::Tap, Format::GoTestJson, Format::LibtestJson, Format::Trx, Format::Xunit, Format::MochaJson, Format::Ctrf];

    pub fn name(self) -> &'static str {
        match self {
            Format::Junit => "junit",
            Format::Tap => "tap",
            Format::GoTestJson => "go-test-json",
            Format::LibtestJson => "libtest-json",
            Format::Trx => "trx",
            Format::Xunit => "xunit",
            Format::MochaJson => "mocha-json",
            Format::Ctrf => "ctrf",
        }
    }

    /// A format by its name, as `test-results: { format }` writes it.
    pub fn parse(s: &str) -> Option<Format> {
        let s = s.trim().to_ascii_lowercase();
        Format::ALL.into_iter().find(|f| f.name() == s).or(match s.as_str() {
            "junit-xml" | "surefire" | "pytest" | "jest-junit" | "go-junit-report" | "nextest" => Some(Format::Junit),
            "go-json" | "gotest" => Some(Format::GoTestJson),
            "libtest" | "cargo" | "cargo-json" | "nextest-json" => Some(Format::LibtestJson),
            "mocha" => Some(Format::MochaJson),
            "ctrf-json" => Some(Format::Ctrf),
            _ => None,
        })
    }
}

/// What a report says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub format: Format,
    pub cases: Vec<Case>,
}

impl Report {
    pub fn count(&self, o: Outcome) -> usize {
        self.cases.iter().filter(|c| c.outcome == o).count()
    }
}

/// Which format `text` is in, from its first bytes.
pub fn detect(text: &str) -> Option<Format> {
    let t = text.trim_start_matches('\u{feff}').trim_start();
    if t.starts_with('<') {
        let root = xml::parse(t).ok()?;
        return match xml::local(&root.name) {
            "testsuites" | "testsuite" => Some(Format::Junit),
            "TestRun" => Some(Format::Trx),
            "assemblies" | "assembly" => Some(Format::Xunit),
            _ => None,
        };
    }
    if t.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(t) {
            if v.pointer("/results/tests").is_some() {
                return Some(Format::Ctrf);
            }
            if v.get("stats").is_some() && (v.get("tests").is_some() || v.get("passes").is_some()) {
                return Some(Format::MochaJson);
            }
        }
        // One object a line: go test's events, or libtest's.
        let first = t.lines().find_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())?;
        if first.get("Action").is_some() {
            return Some(Format::GoTestJson);
        }
        if first.get("type").is_some() && first.get("event").is_some() {
            return Some(Format::LibtestJson);
        }
        return None;
    }
    let tap = t.lines().map(str::trim_start).take(50).any(|l| l.starts_with("TAP version") || l.starts_with("1..") || l.starts_with("ok ") || l.starts_with("not ok "));
    tap.then_some(Format::Tap)
}

/// Reads a report, in `format` or the one [`detect`] finds.
pub fn parse(text: &str, format: Option<Format>) -> Result<Report, String> {
    let text = text.trim_start_matches('\u{feff}');
    let format = match format {
        Some(f) => f,
        None => detect(text).ok_or("not a test report kiln reads: JUnit XML, TAP, go test -json, libtest JSON, TRX, xUnit, Mocha JSON or CTRF")?,
    };
    let cases = match format {
        Format::Junit => junit::junit(text)?,
        Format::Trx => junit::trx(text)?,
        Format::Xunit => junit::xunit(text)?,
        Format::Tap => lines::tap(text)?,
        Format::GoTestJson => lines::go_test_json(text)?,
        Format::LibtestJson => lines::libtest_json(text)?,
        Format::MochaJson => objects::mocha(text)?,
        Format::Ctrf => objects::ctrf(text)?,
    };
    Ok(Report { format, cases })
}

/// What a test's results say about it (W16-R04).
#[derive(Debug, Clone, PartialEq)]
pub enum Flakiness {
    /// It both passed and failed on the same input: across attempts, legs or a tool's retries.
    Flaky { why: String },
    /// It flips on trunk more often than `threshold` with no change that touches it.
    ProbablyFlaky { flip_rate: f64, runs: usize },
    Stable,
}

/// A result of a test as history keeps it, oldest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    /// What decides the result: W08-R10's input hash of the leg (the same commit and inputs).
    pub input: String,
    pub outcome: Outcome,
    /// The tool retried it within the run before it passed.
    pub retried: u32,
}

/// W16-R04: `flaky` when the same test on the same input both passed and failed (or a tool's
/// retry passed it); `probably flaky` when its outcome flipped between consecutive trunk runs in
/// more than `threshold` of the last `window` (with no relevant change: the caller passes only
/// those runs); stable otherwise.
pub fn flaky(results: &[Seen], window: usize, threshold: f64) -> Flakiness {
    if let Some(r) = results.iter().find(|r| r.retried > 0 && r.outcome == Outcome::Passed) {
        return Flakiness::Flaky { why: format!("it failed and then passed on the tool's retry ({} retr{}) on input {}", r.retried, if r.retried == 1 { "y" } else { "ies" }, short(&r.input)) };
    }
    let mut by_input: std::collections::BTreeMap<&str, (bool, bool)> = std::collections::BTreeMap::new();
    for r in results.iter().filter(|r| r.outcome != Outcome::Skipped) {
        let e = by_input.entry(r.input.as_str()).or_default();
        if r.outcome == Outcome::Passed {
            e.0 = true;
        } else {
            e.1 = true;
        }
    }
    if let Some((input, _)) = by_input.iter().find(|(_, (passed, failed))| *passed && *failed) {
        return Flakiness::Flaky { why: format!("it passed and failed on the same input ({})", short(input)) };
    }
    let recent: Vec<bool> = results.iter().filter(|r| r.outcome != Outcome::Skipped).map(|r| r.outcome.is_failure()).collect();
    let recent = recent.get(recent.len().saturating_sub(window)..).unwrap_or_default();
    if recent.len() >= 2 {
        let flips = recent.windows(2).filter(|w| w.first() != w.last()).count();
        let rate = flips as f64 / (recent.len() - 1) as f64;
        // A test that started failing and stays failing flipped once: that's a change, not flakiness.
        if rate > threshold && flips > 1 {
            return Flakiness::ProbablyFlaky { flip_rate: rate, runs: recent.len() };
        }
    }
    Flakiness::Stable
}

fn short(s: &str) -> &str {
    s.get(..12).unwrap_or(s)
}

#[cfg(test)]
mod tests;
