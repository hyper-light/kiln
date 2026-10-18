//! Rollouts (docs/cd-targets.md §5, D9): what an app's `rollout:` says, compiled into steps, and
//! the state machine that walks them. Like the engine, it has no clock, disk or network: it is
//! told what happened (time passed, the platform was read, traffic moved, a check measured, a
//! person promoted) and says what to do (move traffic, measure, stop measuring, let a version
//! go). The server does that, records the rollout after each event, and resumes it after a
//! restart.
//!
//! It does what Argo Rollouts' controller does (its code was read for this, §5 compares them),
//! the same way on every target:
//!
//! - **Traffic follows the step.** The step says what share green should have; the rollout moves
//!   it there, and back there when something else moved it. Green never has traffic while the
//!   platform says it isn't ready: its share goes to blue until it is.
//! - **Progress has a deadline.** Waiting on the platform (green getting ready, traffic moving)
//!   longer than `progress-deadline` (10m) aborts. Waiting for a person or a check doesn't count.
//! - **Checks** take `count` measurements `every` apart after `delay`, each within `timeout`. A
//!   measurement passes, fails, can't tell (inconclusive) or can't measure (an error). `failures`
//!   and `inconclusive` say how many are tolerated, `errors` how many errors in a row (each tried
//!   again 10s on), and `streak` how many passes in a row pass a check early. The worst verdict
//!   wins: failed, then erred, then inconclusive. A failure or an error aborts; an inconclusive
//!   check stops its checks and pauses for a person. `dry-run` checks are measured and shown,
//!   and never decide.
//! - **Background checks** (`watch:`) measure from a step to the end.
//! - **Blue/green** runs its `before` checks against green's preview while blue keeps all the
//!   traffic, promotes (by itself, after a while, or when a person says so), optionally ramps,
//!   then watches its `after` checks against production for its window. Blue stays warm for
//!   `keep`, and green for `keep-failed` after an abort.
//! - **People** pause, resume, promote (past the step it is at), promote fully (past every step
//!   and check), abort and retry. A newer revision supersedes a rollout: traffic goes back to
//!   blue, and the newer one starts from the first step.
//! - **Every ask has an id**, so a late answer to an old ask is ignored, and after a restart what
//!   was in flight is asked again with the same ids.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::apps::duration_ms;

/// Measurements a check keeps, the latest (Argo Rollouts keeps ten); its tally counts them all.
const KEEP_MEASUREMENTS: usize = 10;
/// A measurement that erred is taken again this soon (Argo Rollouts' retry interval).
const ERROR_RETRY_MS: u64 = 10_000;
/// Errors in a row a check tolerates unless it says (Argo Rollouts' `consecutiveErrorLimit`).
const ERRORS_IN_A_ROW: u32 = 4;
/// How long the platform may take to make green ready or move traffic (Argo Rollouts'
/// `progressDeadlineSeconds`).
const PROGRESS_DEADLINE_MS: u64 = 600_000;
/// How long blue stays after a canary (Argo Rollouts' `scaleDownDelaySeconds`), and green after
/// an abort (`abortScaleDownDelaySeconds`): long enough for connections to drain.
const DRAIN_MS: u64 = 30_000;
/// How long a blue/green keeps blue warm, and how long its after checks watch production.
const KEEP_MS: u64 = 1_800_000;
const WINDOW_MS: u64 = 600_000;
/// Switching back to blue never gives up: a refusal is tried again this soon.
const REVERT_RETRY_MS: u64 = 10_000;
/// Asked again when the server hasn't answered this long: a switch back to blue, letting a
/// version go.
const UNANSWERED_MS: u64 = 60_000;
/// Entries the record keeps: the first ones, and the latest. Those between are dropped, and
/// counted.
const LOG_LIMIT: usize = 200;
const LOG_HEAD: usize = 20;

/// Green's share as the server reads it when traffic goes to a version neither blue nor green:
/// whatever the step wants, the rollout moves traffic back to its split.
pub const FOREIGN: u8 = 255;

// ------------------------------------------------------------------ what the deploy file says

/// An app's `rollout:`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct RolloutSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canary: Option<Vec<StepSpec>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linear: Option<LinearSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blue_green: Option<BlueGreenSpec>,
    /// Checks measured in the background, from a canary step to the end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch: Option<WatchSpec>,
    /// How long the platform may take to make green ready or move traffic before the rollout
    /// aborts: `10m`, or `none`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_deadline: Option<String>,
    /// How long green stays after an abort: `30s`, or longer to look at it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_failed: Option<String>,
    /// Going back to one of the last this-many revisions goes straight to all the traffic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback_window: Option<u32>,
    /// After a failure, hold the app at its previous deployment until git changes it (the
    /// default).
    #[serde(default = "yes")]
    pub auto_rollback: bool,
}

fn yes() -> bool {
    true
}

/// One canary step: any of a weight, checks, a pause and an approval, done in that order.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct StepSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<u8>,
    /// Checks to pass (`check` or Argo's spelling, `analysis`).
    #[serde(default, alias = "analysis", skip_serializing_if = "Names::is_empty")]
    pub check: Names,
    /// A duration (`5m`), or `promote`: until a person promotes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub approve: bool,
}

/// One check's name, or several.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Names {
    #[default]
    None,
    One(String),
    Many(Vec<String>),
}

impl Names {
    pub fn is_empty(&self) -> bool {
        self.list().is_empty()
    }

    pub fn list(&self) -> Vec<String> {
        match self {
            Names::None => Vec::new(),
            Names::One(n) => vec![n.clone()],
            Names::Many(v) => v.clone(),
        }
    }
}

/// `linear: { step: 10%, every: 2m, check: … }`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct LinearSpec {
    /// Green's share added at each step: `10%` or `10`.
    pub step: Value,
    pub every: String,
    #[serde(default, alias = "analysis", skip_serializing_if = "Names::is_empty")]
    pub check: Names,
}

/// `watch: { check: errors, from: 2 }`: checks measured from the second canary step to the end
/// (Argo's background analysis and its `startingStep`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct WatchSpec {
    #[serde(default, alias = "analysis", skip_serializing_if = "Names::is_empty")]
    pub check: Names,
    /// The step (from 1) they start at: the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<u32>,
}

/// `blue-green: { before, promote, ramp, after, window, keep, parallel }`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct BlueGreenSpec {
    #[serde(default, skip_serializing_if = "Names::is_empty")]
    pub before: Names,
    /// `auto` (once the before checks pass, the default), `manual` (when a person says so), or a
    /// duration: by itself after that long, sooner if a person says so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promote: Option<String>,
    /// Shares of traffic green takes before all of it, with the after checks at each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ramp: Vec<u8>,
    #[serde(default, skip_serializing_if = "Names::is_empty")]
    pub after: Names,
    /// How long the after checks watch production (10m).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    /// How long blue stays warm (30m).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep: Option<String>,
    /// Run the before checks together, not in order.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub parallel: bool,
}

/// A check (the deploy file's `checks:`): what it measures, how often, and its criteria. What
/// it measures (`http`, `metric`, `script`, `workflow`, `approve`) is the server's to do; this
/// holds it as written.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct CheckSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metric: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approve: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay: Option<String>,
    /// Each measurement's limit, or `none`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
    /// Failures tolerated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failures: Option<u32>,
    /// Inconclusive measurements tolerated before a person decides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inconclusive: Option<u32>,
    /// Errors (it couldn't measure) tolerated in a row: 4.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errors: Option<u32>,
    /// Passes in a row that pass it early.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub streak: Option<u32>,
    /// Measured and shown; never decides.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dry_run: bool,
    /// `${{ }}` expressions over `result` and `rollout`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pass: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail: Option<String>,
}

impl CheckSpec {
    /// What it measures: `http`, `metric`, `script`, `workflow` or `approve`.
    pub fn kind(&self) -> Result<&'static str, String> {
        let kinds: Vec<&'static str> = [("http", self.http.is_some()), ("metric", self.metric.is_some()), ("script", self.script.is_some()), ("workflow", self.workflow.is_some()), ("approve", self.approve.is_some())]
            .into_iter()
            .filter(|(_, on)| *on)
            .map(|(k, _)| k)
            .collect();
        match kinds.as_slice() {
            [one] => Ok(one),
            [] => Err("a check measures one thing: http, metric, script, workflow or approve".into()),
            many => Err(format!("a check measures one thing, not {}", many.join(" and "))),
        }
    }

    /// How it measures. `until_stopped`: in a window or the background, where a check without
    /// a `count` measures until it is stopped.
    pub fn timing(&self, until_stopped: bool) -> Result<Timing, String> {
        let d = |s: &Option<String>, what: &str, default: u64| -> Result<u64, String> {
            match s {
                None => Ok(default),
                Some(t) => duration_ms(t).ok_or_else(|| format!("{what} `{t}` isn't a duration (30s, 5m, 1h)")),
            }
        };
        let kind = self.kind()?;
        // Scripts, workflows and people answer once by default; probes and queries a few times.
        let once = matches!(kind, "script" | "workflow" | "approve");
        let count = match (self.count, self.streak) {
            (Some(c), _) => Some(c.max(1)),
            (None, Some(s)) if s > 0 => None,
            (None, _) if until_stopped && !once => None,
            (None, _) => Some(if once { 1 } else { 3 }),
        };
        let timeout_ms = match self.timeout.as_deref().map(str::trim) {
            Some("none") => None,
            Some(_) => Some(d(&self.timeout, "timeout", 0)?),
            // Argo's web metric waits 10s; a person waits for no clock.
            None => match kind {
                "http" => Some(10_000),
                "metric" => Some(30_000),
                "script" => Some(1_800_000),
                "workflow" => Some(3_600_000),
                _ => None,
            },
        };
        Ok(Timing {
            count,
            every_ms: d(&self.every, "every", 10_000)?,
            delay_ms: d(&self.delay, "delay", 0)?,
            timeout_ms,
            failures: self.failures.unwrap_or(0),
            inconclusive: self.inconclusive.unwrap_or(0),
            errors: self.errors.unwrap_or(ERRORS_IN_A_ROW),
            streak: self.streak.unwrap_or(0),
            dry_run: self.dry_run,
            lapse_fails: kind == "approve",
        })
    }
}

/// How a check measures, from its spec.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timing {
    /// Measurements that decide it (errors don't count); none: until it is stopped, or its
    /// streak.
    pub count: Option<u32>,
    pub every_ms: u64,
    pub delay_ms: u64,
    /// Each measurement's limit; none: no limit (a person's answer).
    pub timeout_ms: Option<u64>,
    pub failures: u32,
    pub inconclusive: u32,
    /// Errors tolerated in a row.
    pub errors: u32,
    /// Passes in a row that pass it early (0: none).
    pub streak: u32,
    pub dry_run: bool,
    /// An answer that doesn't come in time fails it (an approval nobody gave); any other errs.
    pub lapse_fails: bool,
}

// ------------------------------------------------------------------ compiled steps

/// What a check runs against.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Against {
    /// Green's own address: `${{ rollout.preview_url }}`.
    Preview,
    /// Production, as it is being shared.
    Production,
}

/// One step of a compiled rollout.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "step")]
pub enum Step {
    /// Give green this share of traffic.
    Weight { weight: u8 },
    /// Wait this long, or (none) until a person promotes.
    Pause { ms: Option<u64> },
    /// These checks must pass.
    Checks { names: Vec<String>, against: Against, parallel: bool },
    /// Measure these checks for a whole window; any failure fails the rollout.
    Watch { names: Vec<String>, window_ms: u64 },
    /// A person approves.
    Approve,
}

/// Checks measured from step `from` to the end.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Background {
    pub names: Vec<String>,
    pub from: usize,
}

/// A rollout's steps, and how it treats the versions around it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub steps: Vec<Step>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<Background>,
    /// How long blue stays after success, and green after an abort.
    pub keep_ms: u64,
    pub keep_failed_ms: u64,
    /// How long the platform may take to make green ready or move traffic; none: no limit.
    pub progress_deadline_ms: Option<u64>,
    /// Going back to one of the last this-many revisions goes straight to all the traffic.
    pub rollback_window: u32,
    /// Blue/green: green has a preview before any traffic.
    pub preview: bool,
    pub auto_rollback: bool,
}

fn percent(v: &Value) -> Result<u8, String> {
    let n = match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().trim_end_matches('%').trim().parse::<f64>().ok(),
        _ => None,
    };
    match n {
        Some(p) if p > 0.0 && p <= 100.0 => Ok(p.round() as u8),
        _ => Err(format!("`{v}` isn't a share of traffic (1 to 100, or 10%)")),
    }
}

/// Compiles a rollout: its steps in order, every check named in `checks`.
pub fn compile(spec: &RolloutSpec, checks: &BTreeMap<String, CheckSpec>) -> Result<Plan, String> {
    let given = [spec.canary.is_some(), spec.linear.is_some(), spec.blue_green.is_some()].iter().filter(|x| **x).count();
    if given > 1 {
        return Err("a rollout is one of canary, linear or blue-green".into());
    }
    let dur = |s: &Option<String>, what: &str, default: u64| -> Result<u64, String> {
        match s {
            None => Ok(default),
            Some(t) => duration_ms(t).ok_or_else(|| format!("{what} `{t}`: a duration (30s, 5m, 1h)")),
        }
    };
    let mut steps = Vec::new();
    // Where each step as written begins, for `watch: { from }`.
    let mut starts = Vec::new();
    let mut keep_ms = DRAIN_MS;
    let mut preview = false;
    if let Some(canary) = &spec.canary {
        for s in canary {
            starts.push(steps.len());
            if let Some(w) = s.weight {
                if w == 0 || w > 100 {
                    return Err(format!("weight {w}: green's share, 1 to 100"));
                }
                steps.push(Step::Weight { weight: w });
            }
            if !s.check.is_empty() {
                steps.push(Step::Checks { names: s.check.list(), against: Against::Production, parallel: true });
            }
            match s.pause.as_deref() {
                None => {}
                Some("promote") => steps.push(Step::Pause { ms: None }),
                Some(t) => steps.push(Step::Pause { ms: Some(duration_ms(t).ok_or_else(|| format!("pause `{t}`: a duration (5m), or promote"))?) }),
            }
            if s.approve {
                steps.push(Step::Approve);
            }
        }
    } else if let Some(l) = &spec.linear {
        let p = percent(&l.step)?;
        let every = duration_ms(&l.every).ok_or_else(|| format!("every `{}`: a duration", l.every))?;
        let mut w = 0u8;
        while w < 100 {
            starts.push(steps.len());
            w = w.saturating_add(p).min(100);
            steps.push(Step::Weight { weight: w });
            if !l.check.is_empty() {
                steps.push(Step::Checks { names: l.check.list(), against: Against::Production, parallel: true });
            }
            if w < 100 {
                steps.push(Step::Pause { ms: Some(every) });
            }
        }
    } else if let Some(bg) = &spec.blue_green {
        if spec.watch.is_some() {
            return Err("a blue/green watches production with after:, not watch:".into());
        }
        preview = true;
        if !bg.before.is_empty() {
            steps.push(Step::Checks { names: bg.before.list(), against: Against::Preview, parallel: bg.parallel });
        }
        match bg.promote.as_deref().map(str::trim) {
            None | Some("auto") => {}
            Some("manual") => steps.push(Step::Pause { ms: None }),
            Some(t) => steps.push(Step::Pause { ms: Some(duration_ms(t).ok_or_else(|| format!("promote `{t}`: auto, manual, or a duration (30m)"))?) }),
        }
        let window_ms = dur(&bg.window, "window", WINDOW_MS)?;
        for w in &bg.ramp {
            if *w == 0 || *w >= 100 {
                return Err(format!("ramp {w}: shares below 100; promotion takes the rest"));
            }
            steps.push(Step::Weight { weight: *w });
            if !bg.after.is_empty() {
                steps.push(Step::Checks { names: bg.after.list(), against: Against::Production, parallel: true });
            }
        }
        steps.push(Step::Weight { weight: 100 });
        if !bg.after.is_empty() {
            steps.push(Step::Watch { names: bg.after.list(), window_ms });
        }
        keep_ms = dur(&bg.keep, "keep", KEEP_MS)?;
    }
    // Every rollout ends with all the traffic on green.
    if !steps.iter().any(|s| matches!(s, Step::Weight { weight: 100 })) {
        steps.push(Step::Weight { weight: 100 });
    }
    let background = match &spec.watch {
        Some(w) if !w.check.is_empty() => {
            let from = w.from.unwrap_or(1);
            let at = from.checked_sub(1).and_then(|i| starts.get(i as usize)).copied().ok_or_else(|| format!("watch from step {from}: the rollout has {} steps", starts.len()))?;
            Some(Background { names: w.check.list(), from: at })
        }
        _ => None,
    };
    let progress_deadline_ms = match spec.progress_deadline.as_deref().map(str::trim) {
        Some("none") => None,
        Some(_) => Some(dur(&spec.progress_deadline, "progress-deadline", 0)?),
        None => Some(PROGRESS_DEADLINE_MS),
    };
    let keep_failed_ms = dur(&spec.keep_failed, "keep-failed", DRAIN_MS)?;
    let named = steps.iter().flat_map(|s| match s {
        Step::Checks { names, .. } | Step::Watch { names, .. } => names.as_slice(),
        _ => &[],
    });
    for n in named.chain(background.iter().flat_map(|b| b.names.iter())) {
        let c = checks.get(n).ok_or_else(|| format!("no check named {n} (checks: lists {})", checks.keys().cloned().collect::<Vec<_>>().join(", ")))?;
        c.timing(false).map_err(|e| format!("check {n}: {e}"))?;
    }
    Ok(Plan { steps, background, keep_ms, keep_failed_ms, progress_deadline_ms, rollback_window: spec.rollback_window.unwrap_or(0), preview, auto_rollback: spec.auto_rollback })
}

fn span(ms: u64) -> String {
    match ms {
        0 => "0s".into(),
        m if m % 3_600_000 == 0 => format!("{}h", m / 3_600_000),
        m if m % 60_000 == 0 => format!("{}m", m / 60_000),
        m if m % 1000 == 0 => format!("{}s", m / 1000),
        m => format!("{m}ms"),
    }
}

// ------------------------------------------------------------------ checks under way

/// A measurement's verdict: it passed, failed, couldn't tell, or couldn't measure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    Pass,
    Fail,
    Inconclusive,
    Error,
}

/// One measurement of a check: what was measured, and its verdict.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Measurement {
    pub at_ms: u64,
    pub verdict: Verdict,
    /// What was measured (`result`), as the criteria saw it.
    #[serde(default)]
    pub value: Value,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
}

/// A check's measurements, counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tally {
    pub passed: u32,
    pub failed: u32,
    pub inconclusive: u32,
    pub errors: u32,
    /// Passes in a row, and errors in a row, up to the latest.
    pub streak: u32,
    pub errors_in_a_row: u32,
}

impl Tally {
    /// The measurements that count toward `count`: all but errors.
    pub fn taken(&self) -> u32 {
        self.passed + self.failed + self.inconclusive
    }

    pub fn tries(&self) -> u32 {
        self.taken() + self.errors
    }
}

/// Something asked of the server and not answered yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ask {
    pub id: u64,
    pub at_ms: u64,
}

/// A check under way: its measurements, and its verdict once it has one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CheckRun {
    pub name: String,
    pub timing: Timing,
    pub against: Against,
    /// The latest measurements, oldest first.
    pub measurements: Vec<Measurement>,
    pub tally: Tally,
    /// The measurement asked for and not answered yet.
    pub asked: Option<Ask>,
    /// When the next measurement is due.
    pub next_ms: u64,
    pub verdict: Option<Verdict>,
    /// Why it has its verdict.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub why: String,
}

impl CheckRun {
    fn new(name: &str, timing: Timing, against: Against, now: u64) -> CheckRun {
        CheckRun { name: name.into(), timing, against, measurements: Vec::new(), tally: Tally::default(), asked: None, next_ms: now + timing.delay_ms, verdict: None, why: String::new() }
    }

    /// A check that can't run (it isn't defined, or its spec is wrong): an error at once.
    fn broken(name: &str, against: Against, now: u64, why: String) -> CheckRun {
        let mut c = CheckRun::new(name, Timing::default(), against, now);
        c.verdict = Some(Verdict::Error);
        c.why = format!("can't run: {why}");
        c
    }

    /// Whether its verdict decides anything: a dry run's never does.
    fn counts(&self) -> bool {
        !self.timing.dry_run
    }

    fn due(&self, now: u64) -> bool {
        self.verdict.is_none() && self.asked.is_none() && self.next_ms <= now
    }

    fn record(&mut self, now: u64, m: Measurement) {
        let t = &mut self.tally;
        match m.verdict {
            Verdict::Pass => {
                t.passed += 1;
                t.streak += 1;
                t.errors_in_a_row = 0;
            }
            Verdict::Fail | Verdict::Inconclusive => {
                if m.verdict == Verdict::Fail {
                    t.failed += 1;
                } else {
                    t.inconclusive += 1;
                }
                t.streak = 0;
                t.errors_in_a_row = 0;
            }
            Verdict::Error => {
                t.errors += 1;
                t.errors_in_a_row += 1;
                t.streak = 0;
            }
        }
        self.next_ms = now + if m.verdict == Verdict::Error { ERROR_RETRY_MS } else { self.timing.every_ms };
        self.asked = None;
        self.measurements.push(m);
        if self.measurements.len() > KEEP_MEASUREMENTS {
            self.measurements.remove(0);
        }
        self.judge();
    }

    /// What the last measurement with this verdict said.
    fn last(&self, v: Verdict) -> String {
        self.measurements.iter().rev().find(|m| m.verdict == v).map(|m| if m.message.is_empty() { m.value.to_string() } else { m.message.clone() }).unwrap_or_default()
    }

    /// Its verdict from its tally, the worst first, as Argo assesses a metric.
    fn judge(&mut self) {
        let (t, n) = (self.timing, self.tally);
        let (verdict, why) = if n.failed > t.failures {
            (Verdict::Fail, format!("failed: {} (failures: {} of {} tolerated)", self.last(Verdict::Fail), n.failed, t.failures))
        } else if n.errors_in_a_row > t.errors {
            (Verdict::Error, format!("couldn't measure {} times in a row: {}", n.errors_in_a_row, self.last(Verdict::Error)))
        } else if n.inconclusive > t.inconclusive {
            (Verdict::Inconclusive, format!("can't tell: {} ({} of {} tolerated)", self.last(Verdict::Inconclusive), n.inconclusive, t.inconclusive))
        } else if t.streak > 0 && n.streak >= t.streak {
            (Verdict::Pass, format!("passed {} times in a row", n.streak))
        } else if let Some(count) = t.count.filter(|c| n.taken() >= *c) {
            if t.streak > 0 { (Verdict::Inconclusive, format!("can't tell: never passed {} times in a row in {count}", t.streak)) } else { (Verdict::Pass, format!("{} of {count} passed", n.passed)) }
        } else {
            return;
        };
        self.verdict = Some(verdict);
        self.why = why;
    }

    /// Stopped while measuring, its window over: it passed, nothing having counted against it
    /// (Argo ends an analysis it terminates as successful).
    fn stop(&mut self) {
        if self.verdict.is_none() {
            self.verdict = Some(Verdict::Pass);
            self.why = format!("{} measured, nothing against it", self.tally.taken());
        }
    }

    fn summary(&self) -> Value {
        json!({ "check": self.name, "verdict": self.verdict, "why": self.why, "tally": self.tally, "dry_run": self.timing.dry_run })
    }
}

/// Whether one of these checks can't tell, which stops them until a person decides.
fn unclear(runs: &[CheckRun]) -> Option<&CheckRun> {
    runs.iter().find(|c| c.counts() && c.verdict == Some(Verdict::Inconclusive))
}

// ------------------------------------------------------------------ the state machine

/// Where a rollout stands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "state")]
pub enum State {
    Running,
    /// Waiting for a person: to promote, to approve, or to decide a check that can't tell.
    Paused { why: String },
    Succeeded,
    /// Failed or aborted: traffic goes back to blue.
    Aborted { why: String },
    /// A newer revision came: traffic goes back to blue, and that one starts from the first step.
    Superseded { by: String },
}

/// The versions a rollout is between.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Version {
    /// What had the traffic before.
    Blue,
    /// What is rolling out.
    Green,
}

/// What the server is told.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// Time passed.
    Tick,
    /// The server restarted: what was asked and not answered is asked again, with the same ids.
    Restarted,
    /// The platform, as the server last read it: green's share of traffic (when it can tell),
    /// and whether green is ready for traffic, or why not. A platform that can't say whether
    /// green is ready says it is.
    Observed { weight: Option<u8>, ready: bool, why: String },
    /// Traffic moved as asked.
    Moved { id: u64 },
    /// Traffic couldn't move: the platform said why.
    MoveFailed { id: u64, why: String },
    /// A check measured.
    Measured { id: u64, measurement: Measurement },
    /// A person promotes: past the step it is at (a pause, an approval, a check that can't tell,
    /// checks still measuring), lifting their pause. `full`: past every step and check.
    Promote { full: bool },
    /// A person pauses it: it stops going on, and its checks keep measuring.
    Pause,
    /// A person lifts their pause.
    Resume,
    /// A person aborts it.
    Abort { why: String },
    /// Git moved on: a newer revision rolls out instead.
    Supersede { revision: String },
    /// A person tries an aborted rollout again, from the first step.
    Retry,
    /// A version was let go.
    Released { version: Version },
}

/// What the server is told to do.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "do")]
pub enum Action {
    /// Give green this share of traffic (0: all of it to blue); say [`Event::Moved`] or
    /// [`Event::MoveFailed`] with the id once the platform has it, read back.
    Move { weight: u8, id: u64 },
    /// Measure a check (its n-th try) against preview or production; say [`Event::Measured`].
    Measure { check: String, n: u32, against: Against, id: u64 },
    /// Stop that measurement (cancel the script or workflow): it ran out of time, or its step
    /// ended. An answer that still comes is ignored.
    Cancel { id: u64 },
    /// Let a version go the platform's way (blue after `keep`, green after an abort); say
    /// [`Event::Released`].
    Release { version: Version },
}

/// A move asked for and not answered yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Moving {
    pub weight: u8,
    pub id: u64,
    pub at_ms: u64,
}

/// A version to let go once it's time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    pub version: Version,
    pub at_ms: u64,
    pub asked_ms: Option<u64>,
    pub done: bool,
}

/// A rollout under way: its plan, where it is, and what happened.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rollout {
    pub plan: Plan,
    /// The step it is at, and when that began.
    pub step: usize,
    pub step_ms: u64,
    pub state: State,
    /// There is a version to go back to: not so on an app's first deploy.
    pub blue: bool,
    /// Green's share of traffic, as last moved or read, and whether the platform's was read yet.
    pub weight: u8,
    #[serde(default)]
    pub read: bool,
    pub moving: Option<Moving>,
    /// Green as the platform last said: ready for traffic, or why not.
    pub ready: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub unready: String,
    /// When it last made progress on the platform, for `progress-deadline`.
    pub progress_ms: u64,
    /// The step's checks, and the background ones.
    pub checks: Vec<CheckRun>,
    pub background: Vec<CheckRun>,
    /// Paused by a person.
    pub held: bool,
    /// Promoted past every step: the background checks don't start again.
    pub full: bool,
    /// Finished, and traffic is where it ends: back on blue after an abort.
    pub reverted: bool,
    /// A refused switch back to blue is tried again then.
    pub retry_ms: Option<u64>,
    pub release: Option<Release>,
    /// The last id given to an ask.
    pub seq: u64,
    /// What happened, in order: the panel's record.
    pub log: Vec<Value>,
    pub dropped: u64,
}

impl Rollout {
    /// A rollout at its first step, green not yet read.
    pub fn new(plan: Plan, now: u64) -> Rollout {
        Rollout {
            plan,
            step: 0,
            step_ms: now,
            state: State::Running,
            blue: true,
            weight: 0,
            read: false,
            moving: None,
            ready: false,
            unready: "not read yet".into(),
            progress_ms: now,
            checks: Vec::new(),
            background: Vec::new(),
            held: false,
            full: false,
            reverted: false,
            retry_ms: None,
            release: None,
            seq: 0,
            log: Vec::new(),
            dropped: 0,
        }
    }

    /// A rollout straight to all the traffic, with no steps or checks, as Argo does: an app's
    /// first deploy (`blue` false: nothing to go back to), or going back to a recent revision
    /// (`rollback-window`).
    pub fn straight(mut plan: Plan, now: u64, blue: bool, why: &str) -> Rollout {
        plan.steps = vec![Step::Weight { weight: 100 }];
        plan.background = None;
        let mut r = Rollout::new(plan, now);
        r.blue = blue;
        r.note(now, json!({ "event": "straight", "why": why }));
        r
    }

    /// Whether it succeeded, aborted or was superseded.
    pub fn finished(&self) -> bool {
        matches!(self.state, State::Succeeded | State::Aborted { .. } | State::Superseded { .. })
    }

    /// Whether nothing is left to do: finished, traffic where it ends, versions let go.
    pub fn done(&self) -> bool {
        self.finished() && (self.state == State::Succeeded || self.reverted) && self.moving.is_none() && self.release.is_none_or(|r| r.done)
    }

    /// Where it stands, as the object it rolls out shows it (`status.rollout`) and as health
    /// reads it: its state and why, green's share, the step it's at, whether a person paused it.
    pub fn summary(&self) -> Value {
        let (state, why) = match &self.state {
            State::Running => ("running", String::new()),
            State::Paused { why } => ("paused", why.clone()),
            State::Succeeded => ("succeeded", String::new()),
            State::Aborted { why } => ("aborted", why.clone()),
            State::Superseded { by } => ("superseded", format!("superseded by {by}")),
        };
        let waiting = if self.finished() {
            String::new()
        } else if !self.ready {
            format!("green isn't ready: {}", self.unready)
        } else if self.moving.is_some() {
            "moving traffic".into()
        } else {
            String::new()
        };
        json!({
            "state": state, "why": why, "held": self.held, "weight": self.weight.min(100), "target": self.target(),
            "step": (self.step + 1).min(self.plan.steps.len()), "steps": self.plan.steps.len(), "waiting": waiting,
        })
    }

    /// Green's share the step wants: that of the last weight step up to it; all of it past the
    /// last step.
    pub fn target(&self) -> u8 {
        if self.step >= self.plan.steps.len() {
            return 100;
        }
        self.plan.steps.iter().take(self.step + 1).rev().find_map(|s| if let Step::Weight { weight } = s { Some(*weight) } else { None }).unwrap_or(0)
    }

    /// Green is ready and has the share the step wants: checks may measure, steps may pass.
    fn settled(&self) -> bool {
        self.ready && self.moving.is_none() && self.weight == self.target()
    }

    /// Waiting on the platform: for green to be ready, or for traffic to move.
    fn waiting_on_platform(&self) -> bool {
        self.state == State::Running && !self.held && (!self.ready || self.moving.is_some())
    }

    /// When it next needs a tick, if before something happens.
    pub fn deadline(&self) -> Option<u64> {
        let mut at: Option<u64> = None;
        let mut take = |t: u64| at = Some(at.map_or(t, |a| a.min(t)));
        if let Some(r) = self.release.filter(|r| !r.done) {
            take(r.asked_ms.map_or(r.at_ms, |a| (a + UNANSWERED_MS).max(r.at_ms)));
        }
        if self.finished() {
            if let Some(t) = self.retry_ms {
                take(t);
            }
            if let Some(m) = self.moving.filter(|_| !self.reverted) {
                take(m.at_ms + UNANSWERED_MS);
            }
            return at;
        }
        if let Some(d) = self.plan.progress_deadline_ms.filter(|_| self.waiting_on_platform()) {
            take(self.progress_ms + d);
        }
        let settled = self.settled();
        for runs in [&self.checks, &self.background] {
            if unclear(runs).is_some() {
                continue;
            }
            for c in runs.iter() {
                match (c.asked, c.timing.timeout_ms) {
                    (Some(a), Some(t)) => take(a.at_ms + t),
                    (None, _) if c.verdict.is_none() && settled => take(c.next_ms),
                    _ => {}
                }
            }
        }
        if self.state == State::Running && !self.held && settled {
            match self.plan.steps.get(self.step) {
                Some(Step::Pause { ms: Some(ms) }) => take(self.step_ms + ms),
                Some(Step::Watch { window_ms, .. }) => take(self.step_ms + window_ms),
                _ => {}
            }
        }
        at
    }

    fn note(&mut self, now: u64, what: Value) {
        if let Value::Object(mut m) = what {
            m.insert("at_ms".into(), Value::from(now));
            self.log.push(Value::Object(m));
            if self.log.len() > LOG_LIMIT {
                self.log.remove(LOG_HEAD);
                self.dropped += 1;
            }
        }
    }

    /// Handles `event` at `now`: what to do.
    pub fn handle(&mut self, now: u64, event: Event, checks: &BTreeMap<String, CheckSpec>) -> Vec<Action> {
        let mut out = Vec::new();
        match event {
            Event::Tick => {}
            Event::Restarted => self.restarted(now, &mut out),
            Event::Observed { weight, ready, why } => self.observed(now, weight, ready, why),
            Event::Moved { id } => self.moved(now, id),
            Event::MoveFailed { id, why } => self.move_failed(now, id, why, &mut out),
            Event::Measured { id, measurement } => self.measured(now, id, measurement),
            Event::Promote { full } => self.promote(now, full, &mut out),
            Event::Pause => {
                if !self.finished() && !self.held {
                    self.held = true;
                    self.note(now, json!({ "event": "paused" }));
                }
            }
            Event::Resume => {
                if self.held {
                    self.held = false;
                    self.progress_ms = now;
                    self.note(now, json!({ "event": "resumed" }));
                }
            }
            Event::Abort { why } => {
                if !self.finished() {
                    self.abort(now, why, &mut out);
                }
            }
            Event::Supersede { revision } => {
                if !self.finished() {
                    self.end_checks(now, &mut out, false);
                    self.note(now, json!({ "event": "superseded", "by": revision, "step": self.step }));
                    self.state = State::Superseded { by: revision };
                    self.held = false;
                }
            }
            Event::Retry => self.retry(now),
            Event::Released { version } => {
                if let Some(r) = self.release.as_mut().filter(|r| r.version == version && !r.done) {
                    r.done = true;
                    self.note(now, json!({ "event": "released", "version": version }));
                }
            }
        }
        self.advance(now, checks, &mut out);
        out
    }

    fn restarted(&mut self, now: u64, out: &mut Vec<Action>) {
        if let Some(m) = self.moving.as_mut() {
            m.at_ms = now;
            out.push(Action::Move { weight: m.weight, id: m.id });
        }
        for c in self.checks.iter_mut().chain(self.background.iter_mut()) {
            if let Some(a) = c.asked.as_mut() {
                a.at_ms = now;
                out.push(Action::Measure { check: c.name.clone(), n: c.tally.tries() + 1, against: c.against, id: a.id });
            }
        }
        if let Some(r) = self.release.as_mut() {
            r.asked_ms = None;
        }
        self.progress_ms = now;
    }

    fn observed(&mut self, now: u64, weight: Option<u8>, ready: bool, why: String) {
        if ready != self.ready {
            self.note(now, if ready { json!({ "event": "ready" }) } else { json!({ "event": "unready", "why": why }) });
            self.progress_ms = now;
        }
        self.ready = ready;
        self.unready = if ready { String::new() } else { why };
        // Traffic moved by something else: taken as it is, and moved back while the rollout
        // steers (after it, it is the app's drift, as any). The first reading is where it starts.
        let steering = !self.finished() || !(self.reverted || self.state == State::Succeeded);
        if let Some(w) = weight.filter(|w| *w != self.weight && self.moving.is_none() && steering) {
            if w == FOREIGN {
                self.note(now, json!({ "event": "drifted", "from": self.weight, "why": "traffic went to a version neither blue nor green" }));
            } else {
                self.note(now, json!({ "event": if self.read { "drifted" } else { "read" }, "from": self.weight, "to": w }));
            }
            self.weight = w;
        }
        self.read |= weight.is_some();
    }

    fn moved(&mut self, now: u64, id: u64) {
        let Some(m) = self.moving.filter(|m| m.id == id) else { return };
        self.moving = None;
        let was = self.weight;
        self.weight = m.weight;
        self.read = true;
        self.progress_ms = now;
        self.note(now, json!({ "event": "moved", "weight": m.weight }));
        // An after-promotion window watches green with the traffic: given back after green lost
        // it, the window starts again.
        if !self.finished() && m.weight > was && m.weight == self.target() && matches!(self.plan.steps.get(self.step), Some(Step::Watch { .. })) {
            self.step_ms = now;
            self.note(now, json!({ "event": "window-restarted" }));
        }
    }

    fn move_failed(&mut self, now: u64, id: u64, why: String, out: &mut Vec<Action>) {
        let Some(m) = self.moving.filter(|m| m.id == id) else { return };
        self.moving = None;
        if self.finished() {
            // Switching back to blue doesn't give up.
            self.retry_ms = Some(now + REVERT_RETRY_MS);
            self.note(now, json!({ "event": "move-failed", "weight": m.weight, "why": why }));
        } else {
            self.abort(now, format!("traffic couldn't move to {}%: {why}", m.weight), out);
        }
    }

    fn measured(&mut self, now: u64, id: u64, m: Measurement) {
        // An answer to an ask that's gone (its step ended, it ran out of time) is ignored.
        let Some(c) = self.checks.iter_mut().chain(self.background.iter_mut()).find(|c| c.asked.is_some_and(|a| a.id == id)) else { return };
        let verdict = m.verdict;
        let (value, message) = (m.value.clone(), m.message.clone());
        c.record(now, m);
        // Passes are counted; anything else, and a verdict, is noted.
        let entry = (verdict != Verdict::Pass || c.verdict.is_some())
            .then(|| json!({ "event": "measured", "check": c.name, "n": c.tally.tries(), "verdict": verdict, "value": value, "message": message, "decided": c.verdict, "why": c.why }));
        if let Some(e) = entry {
            self.note(now, e);
        }
    }

    fn promote(&mut self, now: u64, full: bool, out: &mut Vec<Action>) {
        if self.finished() {
            return;
        }
        self.held = false;
        self.progress_ms = now;
        self.note(now, json!({ "event": "promoted", "step": self.step, "full": full }));
        if full {
            self.end_checks(now, out, false);
            self.full = true;
            self.step = self.plan.steps.len();
            self.step_ms = now;
            self.state = State::Running;
            return;
        }
        // A background check that couldn't tell: they measure again, afresh, as Argo starts a
        // new analysis when a person resumes.
        if unclear(&self.background).is_some() {
            self.end_background(now, out, false);
            self.state = State::Running;
            return;
        }
        self.state = State::Running;
        self.next_step(now, out, false);
    }

    fn retry(&mut self, now: u64) {
        if !matches!(self.state, State::Aborted { .. }) {
            return;
        }
        self.note(now, json!({ "event": "retried" }));
        // Green went after keep-failed: the server deploys it again, and it must be ready again.
        if self.release.is_some_and(|r| r.version == Version::Green && (r.done || r.asked_ms.is_some())) {
            self.ready = false;
            self.unready = "deployed again".into();
        }
        self.release = None;
        self.state = State::Running;
        self.step = 0;
        self.step_ms = now;
        self.progress_ms = now;
        self.checks.clear();
        self.background.clear();
        self.full = false;
        self.reverted = false;
        self.retry_ms = None;
    }

    fn abort(&mut self, now: u64, why: String, out: &mut Vec<Action>) {
        self.end_checks(now, out, false);
        self.note(now, json!({ "event": "aborted", "why": why, "step": self.step }));
        self.state = State::Aborted { why };
        self.held = false;
    }

    fn succeed(&mut self, now: u64, out: &mut Vec<Action>) {
        self.end_background(now, out, true);
        self.state = State::Succeeded;
        self.note(now, json!({ "event": "succeeded" }));
        if self.blue {
            self.release = Some(Release { version: Version::Blue, at_ms: now + self.plan.keep_ms, asked_ms: None, done: false });
        }
    }

    fn next_step(&mut self, now: u64, out: &mut Vec<Action>, decided: bool) {
        self.end_step_checks(now, out, decided);
        self.step += 1;
        self.step_ms = now;
        self.progress_ms = now;
    }

    fn end_checks(&mut self, now: u64, out: &mut Vec<Action>, decided: bool) {
        self.end_step_checks(now, out, decided);
        self.end_background(now, out, decided);
    }

    /// Ends the step's checks, stopping what's in flight, and notes how each ended. `decided`:
    /// the step is over as planned, so a check measuring until stopped passed.
    fn end_step_checks(&mut self, now: u64, out: &mut Vec<Action>, decided: bool) {
        for c in std::mem::take(&mut self.checks) {
            self.end_run(now, out, c, decided, false);
        }
    }

    fn end_background(&mut self, now: u64, out: &mut Vec<Action>, decided: bool) {
        for c in std::mem::take(&mut self.background) {
            self.end_run(now, out, c, decided, true);
        }
    }

    fn end_run(&mut self, now: u64, out: &mut Vec<Action>, mut c: CheckRun, decided: bool, background: bool) {
        if let Some(a) = c.asked.take() {
            out.push(Action::Cancel { id: a.id });
        }
        if decided {
            c.stop();
        }
        self.note(now, json!({ "event": "checked", "step": self.step, "background": background, "check": c.summary() }));
    }

    /// Walks the steps as far as they go at `now`.
    fn advance(&mut self, now: u64, specs: &BTreeMap<String, CheckSpec>, out: &mut Vec<Action>) {
        if let Some(d) = self.plan.progress_deadline_ms.filter(|d| !self.finished() && self.waiting_on_platform() && now >= self.progress_ms + d) {
            let why = if self.ready { format!("traffic didn't move within {}", span(d)) } else { format!("green wasn't ready within {}: {}", span(d), self.unready) };
            self.abort(now, why, out);
        }
        // At most a pass per step (and one per check started), so no plan spins.
        for _ in 0..self.plan.steps.len() * 2 + 2 {
            self.steer(now, out);
            if self.finished() || !self.measure(now, specs, out) || self.held || self.state != State::Running || !self.settled() {
                break;
            }
            let Some(step) = self.plan.steps.get(self.step).cloned() else {
                self.succeed(now, out);
                break;
            };
            match step {
                Step::Weight { .. } => self.next_step(now, out, true),
                Step::Pause { ms: Some(ms) } => {
                    if now < self.step_ms + ms {
                        break;
                    }
                    self.next_step(now, out, true);
                }
                Step::Pause { ms: None } => {
                    self.state = State::Paused { why: "waiting for a person to promote".into() };
                    break;
                }
                Step::Approve => {
                    self.state = State::Paused { why: "waiting for approval".into() };
                    break;
                }
                Step::Checks { names, against, parallel } => {
                    if self.start_checks(now, &names, against, parallel, false, specs) {
                        continue;
                    }
                    let passed = |n: &String| self.checks.iter().any(|c| &c.name == n && (c.verdict == Some(Verdict::Pass) || (c.verdict.is_some() && !c.counts())));
                    if !names.iter().all(passed) {
                        break;
                    }
                    self.next_step(now, out, true);
                }
                Step::Watch { names, window_ms } => {
                    if self.start_checks(now, &names, Against::Production, true, true, specs) {
                        continue;
                    }
                    // Over once the window is, and every check with a count has its verdict.
                    if now < self.step_ms + window_ms || self.checks.iter().any(|c| c.verdict.is_none() && c.timing.count.is_some()) {
                        break;
                    }
                    self.next_step(now, out, true);
                }
            }
        }
        self.steer(now, out);
        self.let_go(now, out);
    }

    /// Starts a step's checks: all at once, or in order the next once those before it passed.
    /// Whether it started any.
    fn start_checks(&mut self, now: u64, names: &[String], against: Against, parallel: bool, until_stopped: bool, specs: &BTreeMap<String, CheckSpec>) -> bool {
        let started = |n: &String| self.checks.iter().any(|c| &c.name == n);
        let next: Vec<String> = if parallel {
            names.iter().filter(|n| !started(n)).cloned().collect()
        } else if self.checks.iter().all(|c| c.verdict.is_some()) {
            names.iter().find(|n| !started(n)).cloned().into_iter().collect()
        } else {
            Vec::new()
        };
        for n in &next {
            let run = match specs.get(n).map(|s| s.timing(until_stopped)) {
                Some(Ok(t)) => CheckRun::new(n, t, against, now),
                Some(Err(e)) => CheckRun::broken(n, against, now, e),
                None => CheckRun::broken(n, against, now, "checks: doesn't define it".into()),
            };
            self.checks.push(run);
        }
        !next.is_empty()
    }

    /// Measures the step's checks and the background ones: what ran out of time errs, a failure
    /// or an error aborts, a check that can't tell stops its checks and pauses for a person, and
    /// what's due is asked for while green has the traffic the step wants. Whether it may go on.
    fn measure(&mut self, now: u64, specs: &BTreeMap<String, CheckSpec>, out: &mut Vec<Action>) -> bool {
        if let Some(bg) = self.plan.background.clone().filter(|b| self.background.is_empty() && !self.full && self.step >= b.from && self.step < self.plan.steps.len()) {
            for n in &bg.names {
                self.background.push(match specs.get(n).map(|s| s.timing(true)) {
                    Some(Ok(t)) => CheckRun::new(n, t, Against::Production, now),
                    Some(Err(e)) => CheckRun::broken(n, Against::Production, now, e),
                    None => CheckRun::broken(n, Against::Production, now, "checks: doesn't define it".into()),
                });
            }
            self.note(now, json!({ "event": "watching", "checks": bg.names }));
        }
        let mut lapsed = Vec::new();
        for c in self.checks.iter_mut().chain(self.background.iter_mut()) {
            if let (Some(a), Some(t)) = (c.asked, c.timing.timeout_ms)
                && now >= a.at_ms + t
            {
                out.push(Action::Cancel { id: a.id });
                let verdict = if c.timing.lapse_fails { Verdict::Fail } else { Verdict::Error };
                c.record(now, Measurement { at_ms: now, verdict, value: Value::Null, message: format!("no answer within {}", span(t)) });
                lapsed.push(json!({ "event": "measured", "check": c.name, "n": c.tally.tries(), "verdict": verdict, "message": format!("no answer within {}", span(t)), "decided": c.verdict }));
            }
        }
        for e in lapsed {
            self.note(now, e);
        }
        if let Some(why) = self.checks.iter().chain(&self.background).find(|c| c.counts() && matches!(c.verdict, Some(Verdict::Fail | Verdict::Error))).map(|c| format!("check {} {}", c.name, c.why)) {
            self.abort(now, why, out);
            return false;
        }
        let settled = self.settled();
        let mut seq = self.seq;
        let mut stopped = None;
        for runs in [&mut self.checks, &mut self.background] {
            if let Some(why) = unclear(runs).map(|c| format!("check {} {}: promote to go on, or abort", c.name, c.why)) {
                for c in runs.iter_mut() {
                    if let Some(a) = c.asked.take() {
                        out.push(Action::Cancel { id: a.id });
                    }
                }
                stopped.get_or_insert(why);
                continue;
            }
            if !settled {
                continue;
            }
            for c in runs.iter_mut().filter(|c| c.due(now)) {
                seq += 1;
                c.asked = Some(Ask { id: seq, at_ms: now });
                out.push(Action::Measure { check: c.name.clone(), n: c.tally.tries() + 1, against: c.against, id: seq });
            }
        }
        self.seq = seq;
        match stopped {
            Some(why) => {
                // Shown even while it also waits at a pause or an approval.
                if !matches!(&self.state, State::Paused { why: w } if *w == why) {
                    self.note(now, json!({ "event": "unclear", "why": why }));
                    self.state = State::Paused { why };
                }
                false
            }
            None => true,
        }
    }

    /// Moves traffic toward where it should be: the step's share while green is ready, none of
    /// it otherwise; all of it back on blue once aborted or superseded.
    fn steer(&mut self, now: u64, out: &mut Vec<Action>) {
        let want = match self.state {
            State::Succeeded => return,
            State::Aborted { .. } | State::Superseded { .. } => {
                if self.reverted || !self.blue {
                    self.reverted = true;
                    return;
                }
                0
            }
            State::Running | State::Paused { .. } => {
                if self.ready {
                    self.target()
                } else {
                    0
                }
            }
        };
        if let Some(m) = self.moving {
            // A switch back to blue the platform never answered is asked again.
            if !(self.finished() && now >= m.at_ms + UNANSWERED_MS) {
                return;
            }
            self.moving = None;
        }
        if self.retry_ms.is_some_and(|at| now < at) {
            return;
        }
        self.retry_ms = None;
        if self.weight == want {
            if self.finished() {
                self.reverted = true;
                self.note(now, json!({ "event": "reverted" }));
            }
            return;
        }
        if !self.finished() && !self.ready {
            let why = self.unready.clone();
            self.note(now, json!({ "event": "pulled", "from": self.weight, "why": why }));
        }
        self.seq += 1;
        self.moving = Some(Moving { weight: want, id: self.seq, at_ms: now });
        out.push(Action::Move { weight: want, id: self.seq });
    }

    /// Lets blue go `keep` after success, and green `keep-failed` after traffic went back to
    /// blue (at once when superseded), asking again until the server says it's done.
    fn let_go(&mut self, now: u64, out: &mut Vec<Action>) {
        if self.release.is_none() && self.reverted && self.blue {
            let after = match self.state {
                State::Superseded { .. } => 0,
                _ => self.plan.keep_failed_ms,
            };
            self.release = Some(Release { version: Version::Green, at_ms: now + after, asked_ms: None, done: false });
        }
        if let Some(r) = self.release.as_mut().filter(|r| !r.done && now >= r.at_ms && r.asked_ms.is_none_or(|a| now >= a + UNANSWERED_MS)) {
            r.asked_ms = Some(now);
            out.push(Action::Release { version: r.version });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use serde_json::json;

    use super::*;

    fn checks(text: &str) -> BTreeMap<String, CheckSpec> {
        serde_yaml::from_str(text).unwrap_or_default()
    }

    fn spec(text: &str) -> RolloutSpec {
        serde_yaml::from_str(text).unwrap_or_default()
    }

    fn says(verdict: Verdict, message: &str) -> Option<Measurement> {
        Some(Measurement { at_ms: 0, verdict, value: json!(null), message: message.into() })
    }

    fn pass() -> Option<Measurement> {
        says(Verdict::Pass, "")
    }

    fn fail(why: &str) -> Option<Measurement> {
        says(Verdict::Fail, why)
    }

    /// A server and a platform, simulated: moves land at once, green is ready, measurements are
    /// answered by `answer` (none: never), and time jumps to the rollout's next deadline.
    struct Sim {
        r: Rollout,
        cs: BTreeMap<String, CheckSpec>,
        now: u64,
        seen: Vec<Action>,
    }

    impl Sim {
        fn new(spec_text: &str, checks_text: &str) -> Result<Sim, String> {
            let cs = checks(checks_text);
            let mut r = Rollout::new(compile(&spec(spec_text), &cs)?, 0);
            // The platform was read: green is ready.
            r.ready = true;
            r.unready.clear();
            Ok(Sim { r, cs, now: 0, seen: Vec::new() })
        }

        fn event(&mut self, e: Event) -> Vec<Action> {
            self.r.handle(self.now, e, &self.cs)
        }

        /// Green, ready or not, with the share the rollout last gave it.
        fn observe(&mut self, ready: bool) -> Vec<Action> {
            let w = self.r.weight;
            self.event(Event::Observed { weight: Some(w), ready, why: if ready { String::new() } else { "crash looping".into() } })
        }

        fn run(&mut self, answer: &mut dyn FnMut(&str, u32) -> Option<Measurement>, limit: usize) {
            let first = self.event(Event::Tick);
            self.go(first, answer, limit);
        }

        /// Carries out `queue` and what follows, up to `limit` actions, until nothing is left.
        fn go(&mut self, queue: Vec<Action>, answer: &mut dyn FnMut(&str, u32) -> Option<Measurement>, limit: usize) {
            let mut queue: VecDeque<Action> = queue.into();
            for _ in 0..limit {
                let Some(a) = queue.pop_front() else {
                    match self.r.deadline() {
                        Some(d) => {
                            self.now = self.now.max(d);
                            queue.extend(self.event(Event::Tick));
                            if queue.is_empty() && self.r.deadline() == Some(d) {
                                break;
                            }
                            continue;
                        }
                        None => break,
                    }
                };
                self.seen.push(a.clone());
                let next = match &a {
                    Action::Move { id, .. } => self.event(Event::Moved { id: *id }),
                    Action::Measure { check, n, id, .. } => match answer(check, *n) {
                        Some(m) => self.event(Event::Measured { id: *id, measurement: m }),
                        None => Vec::new(),
                    },
                    Action::Release { version } => self.event(Event::Released { version: *version }),
                    Action::Cancel { .. } => Vec::new(),
                };
                queue.extend(next);
            }
        }

        fn moves(&self) -> Vec<u8> {
            self.seen.iter().filter_map(|a| if let Action::Move { weight, .. } = a { Some(*weight) } else { None }).collect()
        }

        fn measured(&self) -> Vec<&str> {
            self.seen.iter().filter_map(|a| if let Action::Measure { check, .. } = a { Some(check.as_str()) } else { None }).collect()
        }

        fn aborted(&self, with: &str) -> bool {
            matches!(&self.r.state, State::Aborted { why } if why.contains(with))
        }
    }

    #[test]
    fn a_canary_that_passes_reaches_all_the_traffic() -> Result<(), String> {
        let mut s = Sim::new("canary: [{ weight: 10, check: errors, pause: 5m }, { weight: 50 }]", "errors: { metric: { prometheus: { query: up } }, count: 2, every: 1m }")?;
        assert_eq!(s.r.plan.steps.last(), Some(&Step::Weight { weight: 100 }));
        s.run(&mut |_, _| pass(), 100);
        assert_eq!(s.moves(), [10, 50, 100]);
        assert_eq!(s.measured().len(), 2);
        assert_eq!(s.r.state, State::Succeeded);
        assert!(s.now >= 60_000 + 300_000, "the second measurement a minute on, then the pause");
        assert_eq!(s.seen.last(), Some(&Action::Release { version: Version::Blue }), "blue goes once connections drain");
        assert!(s.r.done());
        Ok(())
    }

    #[test]
    fn a_canary_whose_check_fails_switches_back() -> Result<(), String> {
        let mut s = Sim::new("canary: [{ weight: 20, check: errors }, { weight: 60 }]", "errors: { http: { url: x }, count: 3, every: 10s, failures: 1 }")?;
        s.run(&mut |_, n| if n == 1 { pass() } else { fail("HTTP 500") }, 100);
        assert_eq!(s.moves(), [20, 0], "back to blue");
        assert!(s.aborted("errors failed: HTTP 500"), "{:?}", s.r.state);
        assert_eq!(s.seen.last(), Some(&Action::Release { version: Version::Green }), "green goes after keep-failed");
        assert!(s.r.done());
        Ok(())
    }

    /// A blue/green whose before-check fails never gives green any traffic.
    #[test]
    fn blue_green_before_checks_guard_the_switch() -> Result<(), String> {
        let checks = "smoke: { http: { url: x }, count: 2 }\ne2e: { script: { run: ./e2e.sh } }\nerrors: { metric: { prometheus: { query: up } }, every: 1m }";
        let rollout = "blue-green: { before: [smoke, e2e], after: [errors], window: 5m, keep: 1h }";
        let mut s = Sim::new(rollout, checks)?;
        assert!(s.r.plan.preview);
        s.run(&mut |c, _| if c == "e2e" { fail("exit 1") } else { pass() }, 100);
        assert!(s.moves().is_empty(), "green never took traffic");
        assert_eq!(s.measured(), ["smoke", "smoke", "e2e"], "in order: e2e once smoke passed");
        assert!(s.seen.iter().all(|a| !matches!(a, Action::Measure { against: Against::Production, .. })));
        assert!(s.aborted("e2e failed: exit 1"));

        // When they pass: all the traffic, then the window watches production, then blue goes after keep.
        let mut s = Sim::new(rollout, checks)?;
        s.run(&mut |_, _| pass(), 200);
        assert_eq!(s.moves(), [100]);
        let watched = s.seen.iter().filter(|a| matches!(a, Action::Measure { check, against: Against::Production, .. } if check == "errors")).count();
        assert!(watched >= 5, "measured through the window ({watched})");
        assert_eq!(s.seen.last(), Some(&Action::Release { version: Version::Blue }));
        assert!(s.now >= 300_000 + 3_600_000);
        assert_eq!(s.r.state, State::Succeeded);
        Ok(())
    }

    /// An after-promotion failure switches straight back to blue.
    #[test]
    fn blue_green_after_checks_switch_back() -> Result<(), String> {
        let mut s = Sim::new("blue-green: { after: errors, window: 10m }", "errors: { metric: { prometheus: { query: up } }, every: 1m }")?;
        s.run(&mut |_, n| if n < 4 { pass() } else { fail("12 errors") }, 100);
        assert_eq!(s.moves(), [100, 0]);
        assert!(s.aborted("12 errors"));
        Ok(())
    }

    #[test]
    fn people_promote_approve_and_decide() -> Result<(), String> {
        let mut s = Sim::new("blue-green: { before: smoke, promote: manual }", "smoke: { http: { url: x } }")?;
        s.run(&mut |_, _| says(Verdict::Inconclusive, "502 then 200"), 20);
        assert!(matches!(&s.r.state, State::Paused { why } if why.contains("can't tell")), "{:?}", s.r.state);
        // A person decides it's fine; then the manual promotion waits for them too.
        s.event(Event::Promote { full: false });
        assert!(matches!(&s.r.state, State::Paused { why } if why.contains("promote")));
        assert!(matches!(s.event(Event::Promote { full: false }).as_slice(), [Action::Move { weight: 100, .. }]));

        // A person's pause holds a running rollout; resume goes on.
        let mut s = Sim::new("canary: [{ weight: 10, pause: 1h }]", "")?;
        assert!(matches!(s.event(Event::Tick).as_slice(), [Action::Move { weight: 10, id: 1 }]));
        s.event(Event::Moved { id: 1 });
        s.event(Event::Pause);
        s.now = 7_200_000;
        assert!(s.event(Event::Tick).is_empty(), "held past the pause's end");
        assert!(matches!(s.event(Event::Resume).as_slice(), [Action::Move { weight: 100, .. }]));

        // Promoted after a while by itself, or sooner by a person.
        let mut s = Sim::new("blue-green: { promote: 30m }", "")?;
        s.run(&mut |_, _| pass(), 20);
        assert_eq!(s.moves(), [100]);
        assert!(s.r.log.iter().any(|e| e["event"] == "moved" && e["at_ms"] == 1_800_000));
        let mut s = Sim::new("blue-green: { promote: 30m }", "")?;
        assert!(s.event(Event::Tick).is_empty());
        assert!(matches!(s.event(Event::Promote { full: false }).as_slice(), [Action::Move { weight: 100, .. }]));
        Ok(())
    }

    #[test]
    fn linear_steps_and_bad_plans() -> Result<(), String> {
        let cs = checks("e: { http: { url: x } }");
        let plan = compile(&spec("linear: { step: 25%, every: 2m }"), &cs)?;
        let weights: Vec<u8> = plan.steps.iter().filter_map(|s| if let Step::Weight { weight } = s { Some(*weight) } else { None }).collect();
        assert_eq!(weights, [25, 50, 75, 100]);
        assert!(compile(&spec("canary: [{ weight: 10, check: nope }]"), &cs).is_err_and(|e| e.contains("no check named nope")));
        assert!(compile(&spec("canary: []\nlinear: { step: 10, every: 1m }"), &cs).is_err());
        assert!(compile(&RolloutSpec::default(), &checks("x: { http: {}, script: {} }")).is_ok(), "an unused bad check isn't compiled");
        assert!(compile(&spec("canary: [{ weight: 10, check: x }]"), &checks("x: { http: {}, script: {} }")).is_err_and(|e| e.contains("one thing")));
        assert!(compile(&spec("canary: [{ weight: 10 }]\nwatch: { check: e, from: 3 }"), &cs).is_err_and(|e| e.contains("has 1 steps")));
        assert!(compile(&spec("blue-green: {}\nwatch: { check: e }"), &cs).is_err_and(|e| e.contains("after:")));
        assert!(compile(&spec("blue-green: { promote: later }"), &cs).is_err_and(|e| e.contains("auto, manual")));
        assert_eq!(compile(&spec("progress-deadline: none"), &cs)?.progress_deadline_ms, None);
        // A rollout survives a restart: it is plain data.
        let r = Rollout::new(compile(&spec("blue-green: { before: e }"), &cs)?, 5);
        let back: Rollout = serde_json::from_value(serde_json::to_value(&r).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        assert_eq!(back, r);
        Ok(())
    }

    /// Argo's measurement errors: not counted, tried again 10s on, and more than `errors` in a
    /// row abort; a measurement between resets the run.
    #[test]
    fn errors_are_tried_again_then_abort() -> Result<(), String> {
        let mut s = Sim::new("canary: [{ weight: 10, check: q }]", "q: { metric: { prometheus: { query: up } }, count: 2, every: 1m }")?;
        s.run(&mut |_, n| if n <= 4 || n == 6 { says(Verdict::Error, "connection refused") } else { pass() }, 100);
        assert_eq!(s.r.state, State::Succeeded, "four errors, a pass, an error, a pass: {:?}", s.r.state);
        assert_eq!(s.measured().len(), 7);
        let at: Vec<u64> = s.r.log.iter().filter(|e| e["event"] == "measured").filter_map(|e| e["at_ms"].as_u64()).collect();
        assert_eq!(at, [0, 10_000, 20_000, 30_000, 100_000, 110_000], "errors tried again 10s on, a minute after a pass (passes aren't noted until one decides)");

        let mut s = Sim::new("canary: [{ weight: 10, check: q }]", "q: { metric: { prometheus: { query: up } }, errors: 1 }")?;
        s.run(&mut |_, _| says(Verdict::Error, "connection refused"), 100);
        assert!(s.aborted("couldn't measure 2 times in a row: connection refused"), "{:?}", s.r.state);
        assert_eq!(s.moves(), [10, 0]);
        Ok(())
    }

    /// A measurement that doesn't come back in time is stopped and errs; an approval nobody gave
    /// fails.
    #[test]
    fn measurements_run_out_of_time() -> Result<(), String> {
        let mut s = Sim::new("canary: [{ weight: 10, check: probe }]", "probe: { http: { url: x }, errors: 0 }")?;
        s.run(&mut |_, _| None, 100);
        assert!(s.seen.contains(&Action::Cancel { id: 2 }), "the probe in flight is stopped");
        assert!(s.aborted("no answer within 10s"), "{:?}", s.r.state);
        assert!(s.r.log.iter().any(|e| e["event"] == "aborted" && e["at_ms"] == 10_000));

        let mut s = Sim::new("blue-green: { before: ok }", "ok: { approve: { from: [admins] }, timeout: 1h }")?;
        s.run(&mut |_, _| None, 100);
        assert!(s.aborted("ok failed: no answer within 1h"), "{:?}", s.r.state);
        // Without a timeout, a person is waited for.
        let mut s = Sim::new("blue-green: { before: ok }", "ok: { approve: {} }")?;
        s.run(&mut |_, _| None, 100);
        assert_eq!(s.r.state, State::Running);
        assert_eq!(s.r.deadline(), None, "nothing to wake for but the person");
        Ok(())
    }

    /// Argo's consecutiveSuccessLimit, and dry runs that are measured and never decide.
    #[test]
    fn streaks_and_dry_runs() -> Result<(), String> {
        let mut s = Sim::new("canary: [{ weight: 10, check: warm }]", "warm: { http: { url: x }, streak: 3, failures: 2 }")?;
        s.run(&mut |_, n| if n == 2 { fail("cold") } else { pass() }, 100);
        assert_eq!(s.r.state, State::Succeeded);
        assert_eq!(s.measured().len(), 5, "pass, fail, then three passes in a row");

        let mut s = Sim::new("canary: [{ weight: 10, check: warm }]", "warm: { http: { url: x }, streak: 3, count: 4, failures: 2 }")?;
        s.run(&mut |_, n| if n % 2 == 0 { fail("cold") } else { pass() }, 100);
        assert!(matches!(&s.r.state, State::Paused { why } if why.contains("never passed 3 times in a row in 4")), "{:?}", s.r.state);

        let mut s = Sim::new("canary: [{ weight: 10, check: [new, old] }]", "new: { http: { url: x }, dry-run: true }\nold: { http: { url: y } }")?;
        s.run(&mut |c, _| if c == "new" { fail("p99 900ms") } else { pass() }, 100);
        assert_eq!(s.r.state, State::Succeeded, "a dry run's failure doesn't decide");
        assert!(s.r.log.iter().any(|e| e["event"] == "checked" && e.pointer("/check/check") == Some(&json!("new")) && e.pointer("/check/verdict") == Some(&json!("fail")) && e.pointer("/check/dry_run") == Some(&json!(true))));
        Ok(())
    }

    /// Green never has traffic while it isn't ready: nothing moves or measures before it is, its
    /// share goes back to blue when it stops being ready, and the deadline aborts.
    #[test]
    fn green_must_be_ready() -> Result<(), String> {
        let mut s = Sim::new("canary: [{ weight: 20, check: e }]", "e: { http: { url: x }, count: 3, every: 1m }")?;
        s.observe(false);
        assert!(s.event(Event::Tick).is_empty(), "nothing before green is ready");
        assert_eq!(s.r.deadline(), Some(600_000), "the progress deadline");
        s.now = 600_000;
        s.event(Event::Tick);
        assert!(s.aborted("green wasn't ready within 10m: crash looping"), "{:?}", s.r.state);
        assert!(s.r.done() || s.r.release.is_some(), "nothing to switch back");

        let mut s = Sim::new("canary: [{ weight: 20, check: e }]", "e: { http: { url: x }, count: 3, every: 1m }")?;
        let first = s.event(Event::Tick);
        s.go(first, &mut |_, _| None, 2);
        assert_eq!(s.moves(), [20]);
        // Green goes unready with a measurement in flight: its traffic is pulled, nothing more is asked.
        let pulled = s.observe(false);
        assert!(matches!(pulled.as_slice(), [Action::Move { weight: 0, .. }]), "{pulled:?}");
        s.go(pulled, &mut |_, _| None, 1);
        s.now += 120_000;
        assert!(!s.event(Event::Tick).iter().any(|a| matches!(a, Action::Measure { .. })));
        // Ready again: the step's share back, then the checks go on.
        let back = s.observe(true);
        assert!(matches!(back.as_slice(), [Action::Move { weight: 20, .. }]), "{back:?}");
        s.go(back, &mut |_, _| pass(), 100);
        assert_eq!(s.r.state, State::Succeeded);
        assert_eq!(s.moves(), [20, 0, 20, 100]);
        Ok(())
    }

    /// Traffic moved by something else is moved back to where the step has it.
    #[test]
    fn drift_is_moved_back() -> Result<(), String> {
        let mut s = Sim::new("canary: [{ weight: 20, pause: promote }]", "")?;
        s.run(&mut |_, _| pass(), 10);
        assert!(matches!(s.r.state, State::Paused { .. }));
        let back = s.event(Event::Observed { weight: Some(70), ready: true, why: String::new() });
        assert!(matches!(back.as_slice(), [Action::Move { weight: 20, .. }]), "{back:?}");
        assert!(s.r.log.iter().any(|e| e["event"] == "drifted" && e["to"] == 70));
        Ok(())
    }

    /// An answer to an ask that's gone is ignored, even from a check of the same name.
    #[test]
    fn late_answers_are_ignored() -> Result<(), String> {
        let mut s = Sim::new("canary: [{ weight: 10, check: e }, { weight: 30, check: e }]", "e: { http: { url: x }, count: 1 }")?;
        let first = s.event(Event::Tick);
        s.go(first, &mut |_, _| None, 2);
        let Some(Action::Measure { id: old, .. }) = s.seen.last().cloned() else { return Err(format!("{:?}", s.seen)) };
        // A person promotes past the step: its measurement is stopped.
        let next = s.event(Event::Promote { full: false });
        assert!(next.contains(&Action::Cancel { id: old }));
        s.go(next, &mut |_, _| None, 3);
        assert_eq!(s.moves(), [10, 30]);
        // The old answer comes: ignored. The new one decides.
        assert!(s.event(Event::Measured { id: old, measurement: fail("late").ok_or("no")? }).is_empty());
        assert_eq!(s.r.state, State::Running);
        let Some(Action::Measure { id: new, .. }) = s.seen.last().cloned() else { return Err(format!("{:?}", s.seen)) };
        let rest = s.event(Event::Measured { id: new, measurement: pass().ok_or("no")? });
        s.go(rest, &mut |_, _| pass(), 20);
        assert_eq!(s.r.state, State::Succeeded);
        Ok(())
    }

    /// After a restart, what was asked is asked again with the same ids.
    #[test]
    fn a_restart_asks_again() -> Result<(), String> {
        let mut s = Sim::new("canary: [{ weight: 10, check: e }]", "e: { script: { run: ./e2e.sh } }")?;
        let first = s.event(Event::Tick);
        assert!(matches!(first.as_slice(), [Action::Move { weight: 10, id: 1 }]));
        assert!(matches!(s.event(Event::Restarted).as_slice(), [Action::Move { weight: 10, id: 1 }]));
        let measure = s.event(Event::Moved { id: 1 });
        assert!(matches!(measure.as_slice(), [Action::Measure { id: 2, n: 1, .. }]));
        let saved: Rollout = serde_json::from_value(serde_json::to_value(&s.r).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        s.r = saved;
        assert!(matches!(s.event(Event::Restarted).as_slice(), [Action::Measure { id: 2, n: 1, .. }]));
        s.event(Event::Measured { id: 2, measurement: pass().ok_or("no")? });
        assert_eq!(s.r.target(), 100);
        Ok(())
    }

    /// A newer revision supersedes a rollout: traffic back to blue, the green in between let go
    /// at once.
    #[test]
    fn a_newer_revision_supersedes() -> Result<(), String> {
        let mut s = Sim::new("canary: [{ weight: 30, pause: 1h }]", "")?;
        let first = s.event(Event::Tick);
        s.go(first, &mut |_, _| pass(), 1);
        assert_eq!(s.moves(), [30]);
        let back = s.event(Event::Supersede { revision: "b2".into() });
        s.go(back, &mut |_, _| pass(), 10);
        assert_eq!(s.moves(), [30, 0]);
        assert_eq!(s.r.state, State::Superseded { by: "b2".into() });
        assert_eq!(s.seen.last(), Some(&Action::Release { version: Version::Green }));
        assert_eq!(s.now, 0, "at once");
        assert!(s.r.done());
        assert!(s.event(Event::Retry).is_empty() && s.r.finished(), "a superseded rollout isn't tried again");
        Ok(())
    }

    /// Refused moves abort; switching back never gives up; a person can try again from the
    /// first step.
    #[test]
    fn refusals_aborts_and_retries() -> Result<(), String> {
        let mut s = Sim::new("canary: [{ weight: 10 }, { weight: 40 }]", "")?;
        let first = s.event(Event::Tick);
        s.go(first, &mut |_, _| pass(), 1);
        let up = s.event(Event::Tick);
        assert!(up.is_empty(), "{up:?}");
        assert_eq!(s.r.moving.map(|m| m.weight), Some(40));
        let id = s.r.moving.map_or(0, |m| m.id);
        let back = s.event(Event::MoveFailed { id, why: "ResourceConflictException".into() });
        assert!(s.aborted("traffic couldn't move to 40%: ResourceConflictException"));
        let Some(Action::Move { weight: 0, id: back_id }) = back.first().cloned() else { return Err(format!("{back:?}")) };
        assert!(s.event(Event::MoveFailed { id: back_id, why: "throttled".into() }).is_empty());
        assert_eq!(s.r.deadline(), Some(10_000));
        s.now = 10_000;
        let again = s.event(Event::Tick);
        assert!(matches!(again.as_slice(), [Action::Move { weight: 0, .. }]), "{again:?}");
        s.go(again, &mut |_, _| pass(), 5);
        assert!(s.r.reverted);
        assert!(s.r.done(), "green let go after keep-failed");
        // A person tries again, from the first step, once green is deployed again and ready.
        assert!(s.event(Event::Retry).is_empty());
        assert_eq!(s.r.state, State::Running);
        let retried = s.observe(true);
        assert!(matches!(retried.as_slice(), [Action::Move { weight: 10, .. }]), "{retried:?}");
        Ok(())
    }

    /// Promoting fully goes past every step and check to all the traffic.
    #[test]
    fn promoting_fully() -> Result<(), String> {
        let mut s = Sim::new("canary: [{ weight: 10, check: e, pause: 1h }, { weight: 50, approve: true }]\nwatch: { check: bg }", "e: { http: { url: x }, count: 5, every: 1m }\nbg: { metric: { prometheus: { query: up } }, every: 1m }")?;
        let first = s.event(Event::Tick);
        s.go(first, &mut |_, _| None, 3);
        let all = s.event(Event::Promote { full: true });
        assert!(all.iter().filter(|a| matches!(a, Action::Cancel { .. })).count() == 2, "the step's measurement and the background's stopped: {all:?}");
        s.go(all, &mut |_, _| pass(), 20);
        assert_eq!(s.moves(), [10, 100]);
        assert_eq!(s.r.state, State::Succeeded);
        assert!(s.r.background.is_empty());
        Ok(())
    }

    /// Background checks measure from their step to the end; one that fails aborts, one that
    /// can't tell pauses, and a person's promotion measures them again, afresh.
    #[test]
    fn background_checks() -> Result<(), String> {
        let rollout = "canary: [{ weight: 10, pause: 10m }, { weight: 30, pause: 10m }, { weight: 60, pause: 10m }]\nwatch: { check: errors, from: 2 }";
        let cs = "errors: { metric: { prometheus: { query: up } }, every: 1m }";
        let mut s = Sim::new(rollout, cs)?;
        s.run(&mut |_, _| pass(), 400);
        assert_eq!(s.r.state, State::Succeeded);
        let first = s.seen.iter().position(|a| matches!(a, Action::Measure { .. })).unwrap_or(0);
        assert_eq!(s.moves(), [10, 30, 60, 100]);
        assert!(s.seen.iter().take(first).filter(|a| matches!(a, Action::Move { .. })).count() == 2, "from the second step");
        assert_eq!(s.measured().len(), 21, "a minute apart for 20 minutes");

        let mut s = Sim::new(rollout, cs)?;
        s.run(&mut |_, n| if n == 5 { fail("5xx 4%") } else { pass() }, 400);
        assert!(s.aborted("errors failed: 5xx 4%"));
        assert_eq!(s.moves(), [10, 30, 0]);

        let mut s = Sim::new(rollout, cs)?;
        s.run(&mut |_, n| if n == 3 { says(Verdict::Inconclusive, "no data") } else { pass() }, 400);
        assert!(matches!(&s.r.state, State::Paused { why } if why.contains("errors can't tell: no data")), "{:?}", s.r.state);
        let again = s.event(Event::Promote { full: false });
        assert!(matches!(again.as_slice(), [Action::Measure { n: 1, .. }]), "afresh: {again:?}");
        Ok(())
    }

    /// An app's first deploy, and a rollback within the window, go straight to all the traffic.
    #[test]
    fn straight_rollouts() -> Result<(), String> {
        let plan = compile(&spec("canary: [{ weight: 10, pause: 1h }]"), &BTreeMap::new())?;
        let mut first = Rollout::straight(plan.clone(), 0, false, "first deploy");
        let none = first.handle(0, Event::Observed { weight: Some(100), ready: true, why: String::new() }, &BTreeMap::new());
        assert!(none.is_empty(), "the sync gave it everything: {none:?}");
        assert_eq!(first.state, State::Succeeded);
        assert!(first.done(), "no blue to let go");

        let mut back = Rollout::straight(plan, 0, true, "rolling back to r7, within rollback-window");
        let moved = back.handle(0, Event::Observed { weight: Some(0), ready: true, why: String::new() }, &BTreeMap::new());
        assert!(matches!(moved.as_slice(), [Action::Move { weight: 100, id: 1 }]));
        back.handle(0, Event::Moved { id: 1 }, &BTreeMap::new());
        assert_eq!(back.state, State::Succeeded);
        assert!(back.release.is_some_and(|r| r.version == Version::Blue && r.at_ms == DRAIN_MS));
        Ok(())
    }

    /// A long window keeps the latest measurements and a bounded record; its tally counts all.
    #[test]
    fn records_stay_bounded() -> Result<(), String> {
        let mut s = Sim::new("blue-green: { after: errors, window: 3h }", "errors: { metric: { prometheus: { query: up } }, every: 10s }")?;
        let first = s.event(Event::Tick);
        s.go(first, &mut |_, n| if n % 3 == 0 { says(Verdict::Error, "timeout") } else { pass() }, 5000);
        assert_eq!(s.r.state, State::Succeeded);
        assert!(s.r.log.len() <= LOG_LIMIT && s.r.dropped > 0, "{} entries, {} dropped", s.r.log.len(), s.r.dropped);
        let summary = s.r.log.iter().rev().find(|e| e["event"] == "checked").ok_or("no summary")?;
        assert!(summary.pointer("/check/tally/passed").and_then(Value::as_u64).is_some_and(|p| p > 300), "{summary}");
        Ok(())
    }

    /// Green losing its traffic in an after-promotion window starts the window again once it's
    /// back.
    #[test]
    fn the_window_starts_again() -> Result<(), String> {
        let mut s = Sim::new("blue-green: { after: errors, window: 10m }", "errors: { metric: { prometheus: { query: up } }, every: 1m }")?;
        let first = s.event(Event::Tick);
        s.go(first, &mut |_, _| pass(), 12);
        assert_eq!(s.moves(), [100]);
        s.now = 300_000;
        let pulled = s.observe(false);
        s.go(pulled, &mut |_, _| pass(), 1);
        s.now = 360_000;
        let back = s.observe(true);
        s.go(back, &mut |_, _| pass(), 200);
        assert_eq!(s.moves(), [100, 0, 100]);
        assert_eq!(s.r.state, State::Succeeded);
        let succeeded = s.r.log.iter().find(|e| e["event"] == "succeeded").and_then(|e| e["at_ms"].as_u64()).unwrap_or(0);
        assert!(succeeded >= 360_000 + 600_000, "a whole window after green got its traffic back ({succeeded})");
        Ok(())
    }
}
