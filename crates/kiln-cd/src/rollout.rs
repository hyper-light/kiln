//! Rollouts (docs/cd-targets.md §5, D9): what an app's `rollout:` says, compiled into steps, and
//! the state machine that walks them. Like the engine, it has no clock, disk or network: it is
//! told what happened (time passed, a check measured, traffic moved, a person promoted) and says
//! what to do (move traffic, measure a check, wait, ask a person, switch back, finish). The
//! server does that, records the rollout after each step, and resumes it after a restart.
//!
//! - **Canary** and **linear** steps move green's share of traffic, pause, run checks and wait
//!   for approval.
//! - **Blue/green** runs its `before` checks against green's preview while blue keeps all the
//!   traffic (a failure means green never takes any), promotes (by itself, or when a person says
//!   so), optionally ramps, then watches its `after` checks against production for its window (a
//!   failure switches straight back to blue, which is kept warm).
//! - **Checks** take `count` measurements `every` apart after `delay`, tolerating `failures`
//!   misses and `inconclusive` unclear ones; an after-promotion watch measures for its whole
//!   window. Each measurement's verdict comes from the server, which evaluates the check's
//!   criteria (`pass:`, `fail:`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::apps::duration_ms;

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

/// How a blue/green promotes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Promote {
    /// Once the before-promotion checks pass.
    #[default]
    Auto,
    /// When a person (or an allowed agent) says so.
    Manual,
}

/// `blue-green: { before, promote, ramp, after, window, keep, parallel }`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct BlueGreenSpec {
    #[serde(default, skip_serializing_if = "Names::is_empty")]
    pub before: Names,
    #[serde(default)]
    pub promote: Promote,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
    /// Misses tolerated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failures: Option<u32>,
    /// Unclear measurements tolerated before a person decides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inconclusive: Option<u32>,
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

    fn timing(&self) -> Result<Timing, String> {
        let d = |s: &Option<String>, what: &str, default: u64| -> Result<u64, String> {
            match s {
                None => Ok(default),
                Some(t) => duration_ms(t).ok_or_else(|| format!("{what} `{t}` isn't a duration (30s, 5m, 1h)")),
            }
        };
        // Scripts and workflows measure once by default; probes and queries a few times.
        let once = matches!(self.kind()?, "script" | "workflow" | "approve");
        Ok(Timing {
            count: self.count.unwrap_or(if once { 1 } else { 3 }).max(1),
            every_ms: d(&self.every, "every", 10_000)?,
            delay_ms: d(&self.delay, "delay", 0)?,
            failures: self.failures.unwrap_or(0),
            inconclusive: self.inconclusive.unwrap_or(0),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timing {
    pub count: u32,
    pub every_ms: u64,
    pub delay_ms: u64,
    pub failures: u32,
    pub inconclusive: u32,
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

/// A rollout's steps, and how long blue stays warm after it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub steps: Vec<Step>,
    pub keep_ms: u64,
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
    let mut steps = Vec::new();
    let mut keep_ms = 0;
    let mut preview = false;
    if let Some(canary) = &spec.canary {
        for s in canary {
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
        preview = true;
        if !bg.before.is_empty() {
            steps.push(Step::Checks { names: bg.before.list(), against: Against::Preview, parallel: bg.parallel });
        }
        if bg.promote == Promote::Manual {
            steps.push(Step::Pause { ms: None });
        }
        let window_ms = match &bg.window {
            Some(w) => duration_ms(w).ok_or_else(|| format!("window `{w}`: a duration"))?,
            None => 600_000,
        };
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
        keep_ms = match &bg.keep {
            Some(k) => duration_ms(k).ok_or_else(|| format!("keep `{k}`: a duration"))?,
            None => 1_800_000,
        };
    }
    // Every rollout ends with all the traffic on green.
    if !steps.iter().any(|s| matches!(s, Step::Weight { weight: 100 })) {
        steps.push(Step::Weight { weight: 100 });
    }
    for s in &steps {
        let names = match s {
            Step::Checks { names, .. } | Step::Watch { names, .. } => names.as_slice(),
            _ => &[],
        };
        for n in names {
            let c = checks.get(n).ok_or_else(|| format!("no check named {n} (checks: lists {})", checks.keys().cloned().collect::<Vec<_>>().join(", ")))?;
            c.kind()?;
            c.timing()?;
        }
    }
    Ok(Plan { steps, keep_ms, preview, auto_rollback: spec.auto_rollback })
}

// ------------------------------------------------------------------ the state machine

/// A measurement's verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    Pass,
    Fail,
    Inconclusive,
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

/// A check during a step: its measurements so far, and its verdict once it has one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CheckRun {
    pub name: String,
    pub timing: Timing,
    pub measurements: Vec<Measurement>,
    /// A measurement asked for and not answered yet.
    pub waiting: bool,
    /// When the next measurement is due.
    pub next_ms: u64,
    pub verdict: Option<Verdict>,
}

impl CheckRun {
    fn judge(&mut self, until_window: bool) {
        let n = |v: Verdict| self.measurements.iter().filter(|m| m.verdict == v).count() as u32;
        let (fails, unclear) = (n(Verdict::Fail), n(Verdict::Inconclusive));
        self.verdict = if fails > self.timing.failures {
            Some(Verdict::Fail)
        } else if unclear > self.timing.inconclusive {
            Some(Verdict::Inconclusive)
        } else if !until_window && self.measurements.len() as u32 >= self.timing.count {
            Some(Verdict::Pass)
        } else {
            None
        };
    }
}

/// Where a rollout stands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "state")]
pub enum State {
    Running,
    /// Waiting for a person: to promote, to approve, or to decide an unclear check.
    Paused { why: String },
    Succeeded,
    /// Traffic went back to blue.
    Aborted { why: String },
}

/// What the server is told.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// Time passed.
    Tick,
    /// A check measured.
    Measured { check: String, measurement: Measurement },
    /// Green's share of traffic is now `weight`.
    Moved { weight: u8 },
    /// Traffic couldn't move: the platform said why.
    MoveFailed { why: String },
    /// A person promotes: past a pause, an approval, or an unclear check.
    Promote,
    /// A person pauses it.
    Pause,
    /// A person resumes a pause they made.
    Resume,
    /// A person (or git moving on) aborts it.
    Abort { why: String },
    /// Blue was let go after `keep`.
    Released,
}

/// What the server is told to do.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "do")]
pub enum Action {
    /// Give green this share of traffic, then say [`Event::Moved`].
    Move { weight: u8 },
    /// Measure a check (the n-th time), against preview or production; say [`Event::Measured`].
    Measure { check: String, n: u32, against: Against },
    /// All the traffic back to blue: the rollout failed or was aborted.
    Revert,
    /// Done: green has everything. Let blue go at `release_at_ms`.
    Finish { release_at_ms: u64 },
    /// Let blue go now (`keep` passed).
    Release,
}

/// A rollout under way: its plan, where it is, and what happened.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rollout {
    pub plan: Plan,
    /// The step it is at.
    pub step: usize,
    /// When that step began.
    pub step_ms: u64,
    pub state: State,
    /// Green's share of traffic, as last moved.
    pub weight: u8,
    /// A move asked for and not answered yet.
    pub moving: Option<u8>,
    /// The checks of the current step.
    pub checks: Vec<CheckRun>,
    /// Paused by a person (not by a step).
    #[serde(default)]
    pub held: bool,
    /// When blue is let go, once finished.
    #[serde(default)]
    pub release_at_ms: Option<u64>,
    /// What happened, in order: the panel's record.
    #[serde(default)]
    pub log: Vec<Value>,
}

impl Rollout {
    /// A rollout at its first step.
    pub fn new(plan: Plan, now: u64) -> Rollout {
        Rollout { plan, step: 0, step_ms: now, state: State::Running, weight: 0, moving: None, checks: Vec::new(), held: false, release_at_ms: None, log: Vec::new() }
    }

    /// Whether it is over (succeeded, or aborted).
    pub fn over(&self) -> bool {
        matches!(self.state, State::Succeeded | State::Aborted { .. })
    }

    /// When it next needs a tick: the earliest due measurement or pause end.
    pub fn deadline(&self) -> Option<u64> {
        if self.state != State::Running || self.held {
            return self.release_at_ms.filter(|_| self.state == State::Succeeded);
        }
        let mut at: Option<u64> = None;
        let mut take = |t: u64| at = Some(at.map_or(t, |a| a.min(t)));
        for c in self.checks.iter().filter(|c| c.verdict.is_none() && !c.waiting) {
            take(c.next_ms);
        }
        match self.plan.steps.get(self.step) {
            Some(Step::Pause { ms: Some(ms) }) => take(self.step_ms + ms),
            Some(Step::Watch { window_ms, .. }) => take(self.step_ms + window_ms),
            _ => {}
        }
        at
    }

    fn note(&mut self, now: u64, what: Value) {
        if let Value::Object(mut m) = what {
            m.insert("at_ms".into(), Value::from(now));
            self.log.push(Value::Object(m));
        }
    }

    fn abort(&mut self, now: u64, why: String, out: &mut Vec<Action>) {
        self.note(now, serde_json::json!({ "event": "aborted", "why": why, "step": self.step }));
        self.state = State::Aborted { why };
        self.checks.clear();
        self.moving = None;
        out.push(Action::Revert);
    }

    /// Handles `event` at `now`: what to do.
    pub fn handle(&mut self, now: u64, event: Event, checks: &BTreeMap<String, CheckSpec>) -> Vec<Action> {
        let mut out = Vec::new();
        match event {
            Event::Tick => {}
            Event::Released => {
                self.release_at_ms = None;
                self.note(now, serde_json::json!({ "event": "released" }));
                return out;
            }
            Event::Abort { why } => {
                if !self.over() {
                    self.abort(now, why, &mut out);
                }
                return out;
            }
            Event::Pause => {
                if !self.over() {
                    self.held = true;
                    self.state = State::Paused { why: "paused by a person".into() };
                    self.note(now, serde_json::json!({ "event": "paused" }));
                }
                return out;
            }
            Event::Resume => {
                if self.held {
                    self.held = false;
                    self.state = State::Running;
                    self.note(now, serde_json::json!({ "event": "resumed" }));
                }
            }
            Event::Promote => {
                if let State::Paused { .. } = self.state {
                    self.held = false;
                    self.state = State::Running;
                    self.note(now, serde_json::json!({ "event": "promoted", "step": self.step }));
                    // Past whatever waited: a pause, an approval, an unclear check.
                    self.next_step(now);
                }
            }
            Event::Moved { weight } => {
                if self.moving == Some(weight) {
                    self.moving = None;
                    self.weight = weight;
                    self.note(now, serde_json::json!({ "event": "moved", "weight": weight }));
                    if matches!(self.plan.steps.get(self.step), Some(Step::Weight { .. })) {
                        self.next_step(now);
                    }
                }
            }
            Event::MoveFailed { why } => {
                if self.moving.is_some() {
                    self.abort(now, format!("traffic couldn't move: {why}"), &mut out);
                    return out;
                }
            }
            Event::Measured { check, measurement } => {
                let watching = matches!(self.plan.steps.get(self.step), Some(Step::Watch { .. }));
                if let Some(c) = self.checks.iter_mut().find(|c| c.name == check && c.waiting) {
                    c.waiting = false;
                    c.next_ms = now + c.timing.every_ms;
                    c.measurements.push(measurement.clone());
                    c.judge(watching);
                    self.note(now, serde_json::json!({ "event": "measured", "check": check, "verdict": measurement.verdict, "value": measurement.value, "message": measurement.message }));
                }
            }
        }
        self.advance(now, checks, &mut out);
        out
    }

    fn next_step(&mut self, now: u64) {
        self.step += 1;
        self.step_ms = now;
        self.checks.clear();
    }

    /// Walks the steps as far as they go at `now`.
    fn advance(&mut self, now: u64, specs: &BTreeMap<String, CheckSpec>, out: &mut Vec<Action>) {
        if let (State::Succeeded, Some(at)) = (&self.state, self.release_at_ms)
            && now >= at
        {
            self.release_at_ms = None;
            out.push(Action::Release);
            return;
        }
        // At most one pass per step, so a plan can't spin.
        for _ in 0..self.plan.steps.len() + 2 {
            if self.state != State::Running || self.held || self.moving.is_some() {
                return;
            }
            let Some(step) = self.plan.steps.get(self.step).cloned() else {
                let release = now + self.plan.keep_ms;
                self.state = State::Succeeded;
                self.release_at_ms = Some(release);
                self.note(now, serde_json::json!({ "event": "succeeded" }));
                out.push(Action::Finish { release_at_ms: release });
                return;
            };
            match step {
                Step::Weight { weight } => {
                    if self.weight == weight {
                        self.next_step(now);
                        continue;
                    }
                    self.moving = Some(weight);
                    out.push(Action::Move { weight });
                    return;
                }
                Step::Pause { ms: Some(ms) } => {
                    if now >= self.step_ms + ms {
                        self.next_step(now);
                        continue;
                    }
                    return;
                }
                Step::Pause { ms: None } => {
                    self.state = State::Paused { why: "waiting for a person to promote".into() };
                    return;
                }
                Step::Approve => {
                    self.state = State::Paused { why: "waiting for approval".into() };
                    return;
                }
                Step::Checks { names, against, parallel } => {
                    self.start_checks(now, &names, specs, parallel);
                    if !self.drive_checks(now, against, false, out) {
                        return;
                    }
                }
                Step::Watch { names, window_ms } => {
                    self.start_checks(now, &names, specs, true);
                    let over = now >= self.step_ms + window_ms;
                    if !self.drive_checks(now, Against::Production, !over, out) {
                        return;
                    }
                }
            }
        }
    }

    /// Starts a step's checks: all at once, or (in order) the first not yet passed.
    fn start_checks(&mut self, now: u64, names: &[String], specs: &BTreeMap<String, CheckSpec>, parallel: bool) {
        let wanted: Vec<&String> = if parallel { names.iter().collect() } else { names.iter().find(|n| !self.checks.iter().any(|c| &c.name == *n && c.verdict == Some(Verdict::Pass))).into_iter().collect() };
        for n in wanted {
            if self.checks.iter().any(|c| &c.name == n) {
                continue;
            }
            let Some(timing) = specs.get(n).and_then(|s| s.timing().ok()) else { continue };
            self.checks.push(CheckRun { name: n.clone(), timing, measurements: Vec::new(), waiting: false, next_ms: now + timing.delay_ms, verdict: None });
        }
    }

    /// Judges the step's checks and asks for the measurements due: whether the step is done
    /// (it moved on, or the next check in order may start), or waits. `watching` keeps an
    /// after-promotion window measuring until it ends.
    fn drive_checks(&mut self, now: u64, against: Against, watching: bool, out: &mut Vec<Action>) -> bool {
        // A failure aborts; an unclear check pauses for a person.
        if let Some(c) = self.checks.iter().find(|c| c.verdict == Some(Verdict::Fail)) {
            let last = c.measurements.iter().rev().find(|m| m.verdict == Verdict::Fail);
            let why = format!("check {} failed: {}", c.name, last.map(|m| if m.message.is_empty() { m.value.to_string() } else { m.message.clone() }).unwrap_or_default());
            self.abort(now, why, out);
            return false;
        }
        if let Some(c) = self.checks.iter().find(|c| c.verdict == Some(Verdict::Inconclusive)) {
            self.state = State::Paused { why: format!("check {} can't tell: promote to go on, or abort", c.name) };
            return false;
        }
        let is_watch = matches!(self.plan.steps.get(self.step), Some(Step::Watch { .. }));
        if is_watch && !watching {
            // The window ended without a failure.
            self.next_step(now);
            return true;
        }
        if !watching {
            let names: Vec<String> = match self.plan.steps.get(self.step) {
                Some(Step::Checks { names, .. }) => names.clone(),
                _ => Vec::new(),
            };
            let passed = |n: &String| self.checks.iter().any(|c| &c.name == n && c.verdict == Some(Verdict::Pass));
            if names.iter().all(passed) {
                self.next_step(now);
                return true;
            }
            // In order: every check started so far passed, so the next one may start.
            if self.checks.iter().all(|c| c.verdict == Some(Verdict::Pass)) {
                return true;
            }
        }
        for c in self.checks.iter_mut().filter(|c| c.verdict.is_none() && !c.waiting && c.next_ms <= now) {
            if !watching && c.measurements.len() as u32 >= c.timing.count {
                continue;
            }
            c.waiting = true;
            out.push(Action::Measure { check: c.name.clone(), n: c.measurements.len() as u32 + 1, against });
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn checks(text: &str) -> BTreeMap<String, CheckSpec> {
        serde_yaml::from_str(text).unwrap_or_default()
    }

    fn spec(text: &str) -> RolloutSpec {
        serde_yaml::from_str(text).unwrap_or_default()
    }

    fn pass() -> Measurement {
        Measurement { at_ms: 0, verdict: Verdict::Pass, value: json!(200), message: String::new() }
    }

    fn fail(why: &str) -> Measurement {
        Measurement { at_ms: 0, verdict: Verdict::Fail, value: json!(500), message: why.into() }
    }

    /// Runs `r` to quiescence: answers moves at once, measurements with `answer`, and ticks
    /// time forward to the next deadline. The actions in order.
    fn run(r: &mut Rollout, cs: &BTreeMap<String, CheckSpec>, now: &mut u64, answer: &mut dyn FnMut(&str, u32) -> Measurement, limit: usize) -> Vec<Action> {
        let mut seen = Vec::new();
        let mut queue = r.handle(*now, Event::Tick, cs);
        for _ in 0..limit {
            if queue.is_empty() {
                match r.deadline() {
                    Some(d) if !r.over() || r.release_at_ms.is_some() => {
                        *now = (*now).max(d);
                        queue = r.handle(*now, Event::Tick, cs);
                        if queue.is_empty() && r.deadline() == Some(d) && *now >= d {
                            break;
                        }
                        continue;
                    }
                    _ => break,
                }
            }
            let a = queue.remove(0);
            seen.push(a.clone());
            let next = match &a {
                Action::Move { weight } => r.handle(*now, Event::Moved { weight: *weight }, cs),
                Action::Measure { check, n, .. } => {
                    let m = answer(check, *n);
                    r.handle(*now, Event::Measured { check: check.clone(), measurement: m }, cs)
                }
                Action::Release => r.handle(*now, Event::Released, cs),
                _ => Vec::new(),
            };
            queue.extend(next);
        }
        seen
    }

    fn moves(actions: &[Action]) -> Vec<u8> {
        actions.iter().filter_map(|a| if let Action::Move { weight } = a { Some(*weight) } else { None }).collect()
    }

    #[test]
    fn a_canary_that_passes_reaches_all_the_traffic() -> Result<(), String> {
        let cs = checks("errors: { metric: { prometheus: { query: up } }, count: 2, every: 1m }");
        let plan = compile(&spec("canary: [{ weight: 10, check: errors, pause: 5m }, { weight: 50 }]"), &cs)?;
        assert_eq!(plan.steps.last(), Some(&Step::Weight { weight: 100 }));
        let mut r = Rollout::new(plan, 0);
        let mut now = 0;
        let actions = run(&mut r, &cs, &mut now, &mut |_, _| pass(), 100);
        assert_eq!(moves(&actions), [10, 50, 100]);
        assert_eq!(actions.iter().filter(|a| matches!(a, Action::Measure { .. })).count(), 2);
        assert_eq!(r.state, State::Succeeded);
        assert!(now >= 60_000 + 300_000, "the second measurement a minute on, then the pause");
        Ok(())
    }

    #[test]
    fn a_canary_whose_check_fails_switches_back() -> Result<(), String> {
        let cs = checks("errors: { http: { url: x }, count: 3, every: 10s, failures: 1 }");
        let mut r = Rollout::new(compile(&spec("canary: [{ weight: 20, check: errors }, { weight: 60 }]"), &cs)?, 0);
        let mut now = 0;
        let actions = run(&mut r, &cs, &mut now, &mut |_, n| if n == 1 { pass() } else { fail("HTTP 500") }, 100);
        assert_eq!(moves(&actions), [20]);
        assert_eq!(actions.last(), Some(&Action::Revert));
        assert!(matches!(&r.state, State::Aborted { why } if why.contains("errors failed") && why.contains("HTTP 500")));
        Ok(())
    }

    /// A blue/green whose before-check fails never gives green any traffic.
    #[test]
    fn blue_green_before_checks_guard_the_switch() -> Result<(), String> {
        let cs = checks("smoke: { http: { url: x }, count: 2 }\ne2e: { script: { run: ./e2e.sh } }\nerrors: { metric: { prometheus: { query: up } }, every: 1m }");
        let s = spec("blue-green: { before: [smoke, e2e], after: [errors], window: 5m, keep: 1h }");
        let plan = compile(&s, &cs)?;
        assert!(plan.preview);
        let mut r = Rollout::new(plan.clone(), 0);
        let mut now = 0;
        let actions = run(&mut r, &cs, &mut now, &mut |c, _| if c == "e2e" { fail("exit 1") } else { pass() }, 100);
        assert!(moves(&actions).is_empty(), "green never took traffic");
        let measured: Vec<&str> = actions.iter().filter_map(|a| if let Action::Measure { check, against, .. } = a { assert_eq!(*against, Against::Preview); Some(check.as_str()) } else { None }).collect();
        assert_eq!(measured, ["smoke", "smoke", "e2e"], "in order: e2e once smoke passed");
        assert_eq!(actions.last(), Some(&Action::Revert));

        // When they pass: all the traffic, then the window watches production, then blue goes after keep.
        let mut r = Rollout::new(plan, 0);
        let mut now = 0;
        let actions = run(&mut r, &cs, &mut now, &mut |_, _| pass(), 200);
        assert_eq!(moves(&actions), [100]);
        let watched = actions.iter().filter(|a| matches!(a, Action::Measure { check, against: Against::Production, .. } if check == "errors")).count();
        assert!(watched >= 5, "measured through the window ({watched})");
        assert!(actions.iter().any(|a| matches!(a, Action::Finish { .. })));
        assert_eq!(actions.last(), Some(&Action::Release));
        assert_eq!(r.state, State::Succeeded);
        Ok(())
    }

    /// An after-promotion failure switches straight back to blue.
    #[test]
    fn blue_green_after_checks_switch_back() -> Result<(), String> {
        let cs = checks("errors: { metric: { prometheus: { query: up } }, every: 1m }");
        let mut r = Rollout::new(compile(&spec("blue-green: { after: errors, window: 10m }"), &cs)?, 0);
        let mut now = 0;
        let actions = run(&mut r, &cs, &mut now, &mut |_, n| if n < 4 { pass() } else { fail("12 errors") }, 100);
        assert_eq!(moves(&actions), [100]);
        assert_eq!(actions.last(), Some(&Action::Revert));
        assert!(matches!(&r.state, State::Aborted { why } if why.contains("12 errors")));
        Ok(())
    }

    #[test]
    fn people_promote_approve_and_decide() -> Result<(), String> {
        let cs = checks("smoke: { http: { url: x }, inconclusive: 0 }");
        let mut r = Rollout::new(compile(&spec("blue-green: { before: smoke, promote: manual }"), &cs)?, 0);
        let mut now = 0;
        let unclear = Measurement { at_ms: 0, verdict: Verdict::Inconclusive, value: json!(null), message: "timed out".into() };
        run(&mut r, &cs, &mut now, &mut |_, _| unclear.clone(), 20);
        assert!(matches!(&r.state, State::Paused { why } if why.contains("can't tell")));
        // A person decides it's fine; then the manual promotion waits for them too.
        r.handle(now, Event::Promote, &cs);
        assert!(matches!(&r.state, State::Paused { why } if why.contains("promote")));
        let after = r.handle(now, Event::Promote, &cs);
        assert_eq!(after, vec![Action::Move { weight: 100 }]);
        // A person's pause holds a running rollout; resume goes on.
        let mut r = Rollout::new(compile(&spec("canary: [{ weight: 10, pause: 1h }]"), &cs)?, 0);
        let first = r.handle(0, Event::Tick, &cs);
        assert_eq!(first, vec![Action::Move { weight: 10 }]);
        r.handle(0, Event::Moved { weight: 10 }, &cs);
        r.handle(1, Event::Pause, &cs);
        assert!(r.handle(7_200_000, Event::Tick, &cs).is_empty(), "held past the pause's end");
        assert_eq!(r.handle(7_200_000, Event::Resume, &cs), vec![Action::Move { weight: 100 }]);
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
        // A rollout survives a restart: it is plain data.
        let r = Rollout::new(compile(&spec("blue-green: { before: e }"), &cs)?, 5);
        let back: Rollout = serde_json::from_value(serde_json::to_value(&r).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        assert_eq!(back, r);
        Ok(())
    }
}
