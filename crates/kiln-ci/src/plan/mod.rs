//! A run's plan (docs/ci.md W04): which legs run, when, and with what. GitHub-exact for GitHub's
//! semantics (the graph, skips, status functions over needs, matrices, `fail-fast`,
//! `max-parallel`, reusable workflows, outputs, permissions), with kiln's matrix and data-flow
//! additions on top.
//!
//! Pure: the caller supplies the workflow, the event's contexts ([`RunInput`]) and a
//! [`PlanSource`] for what lives elsewhere (called workflows, timing history, the repository's
//! files), and carries out the [`PlanAction`]s: queueing legs, recording what the plan decided
//! without running anything, cancelling. As legs finish, [`RunPlan::on_leg_finished`] advances the
//! plan; jobs are expanded only when what they read is known.
//!
//! - [`matrix`]: matrix expansion.
//! - [`perms`]: token permissions.

pub mod matrix;
pub mod perms;

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::expr::{self, Budget, Condition, Contexts, Dialect, ExprError, ParseOptions, StatusView, Template, Value};
use crate::trigger::pattern::Patterns;
use crate::wf::ir::{Axis, Job, JobSecrets, Matrix, Need, RunsOn, Tpl, Workflow, YVal};
use matrix::Combo;

/// How a leg, a job or a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Conclusion {
    Success,
    Failure,
    Cancelled,
    Skipped,
    TimedOut,
}

impl Conclusion {
    /// As `needs.<job>.result` says it: GitHub's needs results are only `success`, `failure`,
    /// `cancelled` and `skipped` (a timeout is a failure).
    pub fn result(self) -> &'static str {
        match self {
            Conclusion::Success => "success",
            Conclusion::Failure | Conclusion::TimedOut => "failure",
            Conclusion::Cancelled => "cancelled",
            Conclusion::Skipped => "skipped",
        }
    }

    pub fn failed(self) -> bool {
        matches!(self, Conclusion::Failure | Conclusion::TimedOut)
    }
}

/// The event and repository a run starts from (W03 builds it).
#[derive(Debug, Clone, Default)]
pub struct RunInput {
    /// The `github` context.
    pub github: serde_json::Value,
    /// `inputs` (`workflow_dispatch`, `agent`).
    pub inputs: serde_json::Value,
    /// `vars`.
    pub vars: serde_json::Value,
    /// The event's changed files, for `changed()` (W03-R12); `None` when not known.
    pub changed: Option<Arc<Vec<String>>>,
    /// Untrusted runs (W03-R08) get read-only permissions.
    pub trusted: bool,
}

/// What the server and repository decide.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Legs per job in the kiln dialect (`[ci] max_legs`); GitHub workflows are held to 256.
    pub max_legs: usize,
    pub default_permissions: perms::Default,
    /// `[ci] max_job_minutes`: no leg's timeout exceeds it.
    pub max_job_minutes: u32,
    /// `runs-on` for kiln-dialect jobs that don't say (`[ci] default_runs_on`).
    pub default_runs_on: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { max_legs: 1_024, default_permissions: perms::Default::Read, max_job_minutes: 7 * 24 * 60, default_runs_on: vec!["k8s".into()] }
    }
}

/// GitHub's limits on reusable workflows (W04-R24).
pub const MAX_CALL_DEPTH: usize = 10;
pub const MAX_CALLED_WORKFLOWS: usize = 50;
/// Job outputs: 1 MiB per job, 50 MiB per run (W04-R18).
pub const MAX_JOB_OUTPUTS: usize = 1 << 20;
pub const MAX_RUN_OUTPUTS: usize = 50 << 20;

/// What the plan reads from outside.
pub trait PlanSource {
    /// The workflow a job's `uses:` names: at the run's commit for `./` and `$/`, or at the SHA
    /// the ref resolved to when the run started.
    fn workflow(&self, uses: &str) -> Result<Arc<Workflow>, String>;
    /// Recorded durations of a job's shard items, in seconds (W16); `None` when unknown.
    fn durations(&self, job: &str, items: &[String]) -> Vec<Option<f64>> {
        let _ = job;
        vec![None; items.len()]
    }
    /// Files at the run's commit matching a filter pattern (`shard.files`).
    fn files(&self, pattern: &str) -> Vec<String> {
        let _ = pattern;
        Vec::new()
    }
}

/// A source with no called workflows, history or files (for workflows that need none).
pub struct NoSource;

impl PlanSource for NoSource {
    fn workflow(&self, uses: &str) -> Result<Arc<Workflow>, String> {
        Err(format!("`{uses}` can't be read here"))
    }
}

/// `runs-on`, evaluated.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunsOnSpec {
    pub labels: Vec<String>,
    pub group: Option<String>,
}

/// `environment`, evaluated (its `url` is evaluated on the runner, after the steps).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentSpec {
    pub name: String,
    pub url: Option<String>,
    /// `deployment: false`: the environment's rules and secrets, no deployment record.
    pub deployment: bool,
}

/// A concurrency group, evaluated (W05-R06, W05-R07).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupSpec {
    pub group: String,
    pub cancel_in_progress: bool,
    /// `queue: max` (up to 100 pending, in order) rather than GitHub's default `single`.
    pub queue_max: bool,
    /// kiln: this many in progress at once.
    pub limit: Option<u32>,
}

/// The secrets a leg may be given (W09 decides which it gets).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretsSpec {
    /// The run's: a job of the run's own workflow.
    Run,
    /// A called workflow's, named by its caller: each name's template is evaluated with the
    /// caller's secrets.
    Mapped { map: Vec<(String, String)>, from: Box<SecretsSpec> },
    /// None: a called workflow whose caller passed no `secrets`.
    None,
}

/// A secret output a leg may read (W04-R22), delivered as masked environment, never in contexts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretOutput {
    pub job: String,
    pub name: String,
    pub leg: String,
}

/// One leg as the scheduler and the runner get it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LegSpec {
    /// Unique within the run: `build`, `build (linux, 14)`, `call (a) / test`.
    pub key: String,
    /// The job's id, with its callers' (`call/test`).
    pub job: String,
    /// The job's name without the matrix values.
    pub job_name: String,
    /// What the run page shows.
    pub name: String,
    pub matrix: serde_json::Value,
    /// The `strategy` context.
    pub strategy: serde_json::Value,
    /// The `needs` context.
    pub needs: serde_json::Value,
    /// The `inputs` context.
    pub inputs: serde_json::Value,
    pub runs_on: RunsOnSpec,
    /// Constraints over runner facts (W05-R22): the platform's, then `requires:`.
    pub requires: Vec<String>,
    pub timeout_minutes: u32,
    pub continue_on_error: bool,
    pub environment: Option<EnvironmentSpec>,
    pub concurrency: Option<GroupSpec>,
    pub permissions: perms::Resolved,
    /// The job container's image, evaluated (its other keys are evaluated on the runner).
    pub container: Option<String>,
    /// Service containers: id and image (an empty image means the service doesn't start).
    pub services: Vec<(String, String)>,
    pub secrets: SecretsSpec,
    pub secret_outputs: Vec<SecretOutput>,
    /// `env-from`: needed jobs' exported environment, applied before the job's own `env`.
    pub env_from: Vec<(String, String)>,
    /// `run.env` as this leg sees it (W04-R21).
    pub run_env: Vec<(String, String)>,
    /// A `shard` or `chunk` leg's items (`$KILN_SHARD_ITEMS`).
    pub items: Vec<String>,
    /// The leg may run under QEMU user emulation (`emulate:`).
    pub emulated: bool,
    /// The needs outputs and results the job's expressions read, for the run's flow (W04-R23).
    pub reads: Vec<String>,
    /// The workflow whose job this is (steps, env, defaults, outputs), for the runner.
    #[serde(skip)]
    pub workflow: Arc<Workflow>,
    #[serde(skip)]
    pub job_index: usize,
}

impl LegSpec {
    pub fn def(&self) -> Option<&Job> {
        self.workflow.jobs.get(self.job_index)
    }
}

/// What a finished leg reports.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LegReport {
    pub conclusion: Option<Conclusion>,
    /// Job outputs (`jobs.<id>.outputs`), evaluated on the runner.
    pub outputs: Vec<(String, String)>,
    /// kiln typed outputs (`$KILN_OUTPUT_JSON`).
    pub json_outputs: Vec<(String, serde_json::Value)>,
    /// Names of outputs declared `secret: true` that were set (their values stay with W09).
    pub secret_outputs: Vec<String>,
    /// `exports.env`: the exported variables' final values.
    pub env: Vec<(String, String)>,
    /// `$KILN_RUN_ENV` lines.
    pub run_env: Vec<(String, String)>,
}

impl LegReport {
    pub fn of(conclusion: Conclusion) -> Self {
        LegReport { conclusion: Some(conclusion), ..LegReport::default() }
    }
}

/// What the caller does next.
#[derive(Debug, Clone, PartialEq)]
pub enum PlanAction {
    /// Run this leg.
    Queue(Box<LegSpec>),
    /// The plan decided a leg without running it: skipped (`when:`, a per-leg `if:`), cancelled
    /// before it was queued (`fail-fast`, the run cancelled), or failed before it could start.
    Settle { leg: Box<LegSpec>, conclusion: Conclusion, reason: String },
    /// A job finished without legs: skipped, or failed before it had any (its matrix couldn't be
    /// expanded, its called workflow couldn't be read).
    Job { job: String, conclusion: Conclusion, reason: String },
    /// Stop a queued or running leg; `queued_only` leaves it if it already started.
    Cancel { leg: String, reason: String, queued_only: bool },
    /// A warning for the run's annotations (outputs dropped over their limit, …).
    Warn { leg: String, message: String },
    /// The run's conclusion; nothing follows.
    Finish(Conclusion),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobState {
    Waiting,
    Expanded,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LegState {
    Waiting,
    Queued,
    Done(Conclusion),
}

#[derive(Debug, Clone)]
struct Leg {
    spec: LegSpec,
    combo: Combo,
    state: LegState,
    report: LegReport,
    /// Finish order, for "the leg that set it last".
    finished: u64,
    /// For a leg that calls a workflow, the scope it runs.
    called: Option<usize>,
    /// Leg-level needs (W04-R17): (node, leg).
    deps: Vec<(usize, usize)>,
    cancel_sent: bool,
}

#[derive(Debug, Clone)]
enum LegNeed {
    Same,
    Where(Tpl),
}

#[derive(Debug, Clone)]
struct Node {
    scope: usize,
    index: usize,
    /// The job's id within its workflow.
    local: String,
    /// With its callers' (`call/test`).
    id: String,
    needs: Vec<usize>,
    leg_needs: Vec<(usize, LegNeed)>,
    state: JobState,
    legs: Vec<Leg>,
    result: Option<Conclusion>,
    fail_fast: FailFast,
    max_parallel: usize,
    per_axis: Vec<(String, String, usize)>,
}

#[derive(Debug, Clone, Default)]
struct FailFast {
    on: bool,
    /// `axis:<name>`: only legs that share the failed leg's value on it.
    axis: Option<String>,
    /// `scope: run`.
    run: bool,
    /// `cancel: queued`.
    queued_only: bool,
}

#[derive(Debug, Clone)]
struct Scope {
    wf: Arc<Workflow>,
    dialect: Dialect,
    inputs: Value,
    /// Prefixes the ids and keys of its jobs (`call (a) / `), and their id path (`call/`).
    prefix: String,
    id_prefix: String,
    /// The calling leg: (node, leg).
    caller: Option<(usize, usize)>,
    depth: usize,
    perms_cap: Option<perms::Resolved>,
    secrets: SecretsSpec,
    nodes: Vec<usize>,
}

/// A run's plan in progress.
#[derive(Debug, Clone)]
pub struct RunPlan {
    /// `run-name`, evaluated; `None` when the workflow has none (the caller names the run).
    pub name: Option<String>,
    /// The workflow's concurrency group.
    pub concurrency: Option<GroupSpec>,
    input: RunInput,
    github: Value,
    vars: Value,
    settings: Settings,
    scopes: Vec<Scope>,
    nodes: Vec<Node>,
    keys: HashMap<String, (usize, usize)>,
    called: Vec<String>,
    cancelled: bool,
    finished: Option<Conclusion>,
    seq: u64,
    output_bytes: usize,
    run_env: Vec<(usize, String, String)>,
    /// The combinations `exclude` and `where:` removed, by job, with why: for the preview.
    removed: BTreeMap<String, Vec<(serde_json::Value, String)>>,
}

/// One job, as the run page and persistence see it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JobView {
    pub id: String,
    pub state: &'static str,
    pub result: Option<Conclusion>,
    pub legs: Vec<LegView>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LegView {
    pub key: String,
    pub name: String,
    pub state: &'static str,
    pub conclusion: Option<Conclusion>,
}

/// Contexts for one evaluation.
struct Cx {
    values: Vec<(&'static str, Value)>,
    status: StatusView,
    changed: Option<Arc<Vec<String>>>,
}

impl Contexts for Cx {
    fn get(&self, name: &str) -> Option<Value> {
        self.values.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone())
    }

    fn status(&self) -> StatusView {
        self.status
    }

    fn changed(&self, globs: &[String]) -> Result<bool, ExprError> {
        let Some(files) = &self.changed else {
            return Err(ExprError::new("KCI2011", "changed() needs the event's changed files, which this event doesn't have", None));
        };
        let p = Patterns::new(globs).map_err(|e| ExprError::new("KCI2020", e.to_string(), None))?;
        Ok(p.any(files.iter().map(String::as_str)))
    }
}

fn err_at(what: &str, e: &ExprError) -> String {
    format!("{what}: {}", e.message)
}

fn template(t: &Tpl, dialect: Dialect, cx: &Cx, what: &str) -> Result<String, String> {
    let tpl = Template::parse(&t.src, dialect).map_err(|e| err_at(what, &e))?;
    expr::render(&tpl, cx, &mut Budget::default()).map_err(|e| err_at(what, &e))
}

fn template_value(t: &Tpl, dialect: Dialect, cx: &Cx, what: &str) -> Result<Value, String> {
    let tpl = Template::parse(&t.src, dialect).map_err(|e| err_at(what, &e))?;
    expr::eval_value(&tpl, cx, &mut Budget::default()).map_err(|e| err_at(what, &e))
}

fn condition(t: &Tpl, dialect: Dialect, cx: &Cx, what: &str) -> Result<bool, String> {
    let c = Condition::parse(&t.src, &ParseOptions::permissive(dialect)).map_err(|e| err_at(what, &e))?;
    expr::eval_condition(&c, cx, &mut Budget::default()).map_err(|e| err_at(what, &e))
}

/// A value of the model, with its `${{ }}` strings evaluated (a single expression keeps its type).
fn yval(v: &YVal, dialect: Dialect, cx: &Cx, what: &str) -> Result<Value, String> {
    Ok(match v {
        YVal::Null => Value::Null,
        YVal::Bool(b) => Value::Bool(*b),
        YVal::Num(n) => Value::Number(*n),
        YVal::Str(t) if t.has_expr() => template_value(t, dialect, cx, what)?,
        YVal::Str(t) => Value::str(&t.src),
        YVal::Seq(s) => Value::array(s.iter().map(|x| yval(x, dialect, cx, what)).collect::<Result<_, _>>()?),
        YVal::Map(m) => Value::object(m.iter().map(|(k, x)| Ok((k.clone(), yval(x, dialect, cx, what)?))).collect::<Result<Vec<_>, String>>()?),
    })
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::String(s) => !(s.is_empty() || s.eq_ignore_ascii_case("false") || &**s == "0"),
        other => other.truthy(),
    }
}

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => Some(*n),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// `8m`, `30s`, `1h30m`, `90` (seconds) as seconds.
fn seconds(s: &str) -> Option<f64> {
    let s = s.trim();
    if let Ok(n) = s.parse::<f64>() {
        return Some(n);
    }
    let (mut total, mut num) = (0.0f64, String::new());
    for c in s.chars() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
            continue;
        }
        let n: f64 = num.parse().ok()?;
        num.clear();
        total += n * match c {
            's' => 1.0,
            'm' => 60.0,
            'h' => 3_600.0,
            _ => return None,
        };
    }
    num.is_empty().then_some(total)
}

fn json(v: &Value) -> serde_json::Value {
    v.to_json()
}

fn combo_value(c: &Combo) -> Value {
    Value::object(c.iter().cloned())
}

impl RunPlan {
    /// The plan of a new run (W04-R01): `run-name`, the workflow's concurrency group, and the
    /// first actions (every job without needs evaluated, expanded and queued).
    pub fn start(wf: Arc<Workflow>, input: RunInput, settings: Settings, src: &dyn PlanSource) -> Result<(RunPlan, Vec<PlanAction>), String> {
        let dialect = wf.dialect.unwrap_or(Dialect::Gha);
        let github = Value::from_json(&input.github);
        let vars = Value::from_json(&input.vars);
        let inputs = Value::from_json(&input.inputs);
        let cx = Cx { values: vec![("github", github.clone()), ("inputs", inputs.clone()), ("vars", vars.clone())], status: StatusView::default(), changed: None };
        let name = match &wf.run_name {
            Some(t) => Some(template(t, dialect, &cx, "run-name")?).filter(|n| !n.trim().is_empty()),
            None => None,
        };
        let concurrency = match &wf.concurrency {
            Some(c) => Some(group(c, dialect, &cx, "concurrency")?),
            None => None,
        };
        let mut plan = RunPlan {
            name,
            concurrency,
            input,
            github,
            vars,
            settings,
            scopes: Vec::new(),
            nodes: Vec::new(),
            keys: HashMap::new(),
            called: Vec::new(),
            cancelled: false,
            finished: None,
            seq: 0,
            output_bytes: 0,
            run_env: Vec::new(),
            removed: BTreeMap::new(),
        };
        plan.add_scope(wf, dialect, inputs, String::new(), String::new(), None, 0, None, SecretsSpec::Run);
        let mut out = Vec::new();
        plan.advance(src, &mut out);
        Ok((plan, out))
    }

    #[allow(clippy::too_many_arguments)]
    fn add_scope(&mut self, wf: Arc<Workflow>, dialect: Dialect, inputs: Value, prefix: String, id_prefix: String, caller: Option<(usize, usize)>, depth: usize, perms_cap: Option<perms::Resolved>, secrets: SecretsSpec) -> usize {
        let s = self.scopes.len();
        let first = self.nodes.len();
        for (i, j) in wf.jobs.iter().enumerate() {
            let (fail_fast, max_parallel, per_axis) = (FailFast { on: true, ..FailFast::default() }, 0, Vec::new());
            self.nodes.push(Node {
                scope: s,
                index: i,
                local: j.id.clone(),
                id: format!("{id_prefix}{}", j.id),
                needs: Vec::new(),
                leg_needs: Vec::new(),
                state: JobState::Waiting,
                legs: Vec::new(),
                result: None,
                fail_fast,
                max_parallel,
                per_axis,
            });
        }
        let nodes: Vec<usize> = (first..self.nodes.len()).collect();
        for (i, j) in wf.jobs.iter().enumerate() {
            let find = |id: &str| wf.jobs.iter().position(|x| x.id == id).map(|p| first + p);
            let mut needs = Vec::new();
            let mut leg_needs = Vec::new();
            for n in &j.needs {
                match n {
                    Need::Job(id) => needs.extend(find(id).filter(|k| *k != first + i)),
                    Need::Legs { job, matrix_same, filter } => {
                        if let Some(k) = find(job).filter(|k| *k != first + i) {
                            leg_needs.push((k, if *matrix_same { LegNeed::Same } else { filter.clone().map_or(LegNeed::Same, LegNeed::Where) }));
                        }
                    }
                }
            }
            needs.sort_unstable();
            needs.dedup();
            if let Some(node) = self.nodes.get_mut(first + i) {
                node.needs = needs;
                node.leg_needs = leg_needs;
            }
        }
        self.scopes.push(Scope { wf, dialect, inputs, prefix, id_prefix, caller, depth, perms_cap, secrets, nodes });
        s
    }

    /// Whether the plan has finished, and how.
    pub fn finished(&self) -> Option<Conclusion> {
        self.finished
    }

    /// Every job and leg as they stand.
    pub fn jobs(&self) -> Vec<JobView> {
        self.nodes
            .iter()
            .map(|n| JobView {
                id: n.id.clone(),
                state: match n.state {
                    JobState::Waiting => "waiting",
                    JobState::Expanded => "running",
                    JobState::Done => "done",
                },
                result: n.result,
                legs: n
                    .legs
                    .iter()
                    .map(|l| LegView {
                        key: l.spec.key.clone(),
                        name: l.spec.name.clone(),
                        state: match l.state {
                            LegState::Waiting => "waiting",
                            LegState::Queued => "queued",
                            LegState::Done(_) => "done",
                        },
                        conclusion: if let LegState::Done(c) = l.state { Some(c) } else { None },
                    })
                    .collect(),
            })
            .collect()
    }

    /// The combinations `exclude` and `where:` removed, by job, with why (W04-R29).
    pub fn removed(&self) -> &BTreeMap<String, Vec<(serde_json::Value, String)>> {
        &self.removed
    }

    /// A leg finished (W04-R02). Reports for legs the plan doesn't know, or already finished,
    /// change nothing.
    pub fn on_leg_finished(&mut self, key: &str, report: LegReport, src: &dyn PlanSource) -> Vec<PlanAction> {
        let mut out = Vec::new();
        let Some(&(n, l)) = self.keys.get(key) else { return out };
        let conclusion = report.conclusion.unwrap_or(Conclusion::Failure);
        if !matches!(self.leg(n, l).map(|x| x.state), Some(LegState::Waiting | LegState::Queued)) {
            return out;
        }
        let report = self.limit_outputs(key, report, &mut out);
        self.seq += 1;
        let seq = self.seq;
        for (k, v) in &report.run_env {
            self.run_env.push((n, k.clone(), v.clone()));
        }
        if let Some(leg) = self.leg_mut(n, l) {
            leg.state = LegState::Done(conclusion);
            leg.report = report;
            leg.finished = seq;
        }
        self.after_leg(n, l, conclusion, src, &mut out);
        self.advance(src, &mut out);
        out
    }

    /// The run was cancelled: queued legs are stopped, waiting ones never start, and only jobs
    /// whose `if` still holds (`always()`, `cancelled()`) run (GitHub-exact).
    pub fn on_cancel(&mut self, src: &dyn PlanSource) -> Vec<PlanAction> {
        let mut out = Vec::new();
        if self.cancelled || self.finished.is_some() {
            return out;
        }
        self.cancelled = true;
        for n in 0..self.nodes.len() {
            let count = self.nodes.get(n).map_or(0, |x| x.legs.len());
            for l in 0..count {
                self.cancel_leg(n, l, "the run was cancelled", false, &mut out);
            }
        }
        self.advance(src, &mut out);
        out
    }

    fn leg(&self, n: usize, l: usize) -> Option<&Leg> {
        self.nodes.get(n).and_then(|x| x.legs.get(l))
    }

    fn leg_mut(&mut self, n: usize, l: usize) -> Option<&mut Leg> {
        self.nodes.get_mut(n).and_then(|x| x.legs.get_mut(l))
    }

    fn scope_of(&self, n: usize) -> Option<&Scope> {
        self.nodes.get(n).and_then(|x| self.scopes.get(x.scope))
    }

    fn job_def(&self, n: usize) -> Option<(&Job, Dialect)> {
        let node = self.nodes.get(n)?;
        let s = self.scopes.get(node.scope)?;
        Some((s.wf.jobs.get(node.index)?, s.dialect))
    }

    /// Outputs over their limits are dropped with a warning (W04-R18).
    fn limit_outputs(&mut self, key: &str, mut r: LegReport, out: &mut Vec<PlanAction>) -> LegReport {
        let size = r.outputs.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>() + r.json_outputs.iter().map(|(k, v)| k.len() + v.to_string().len()).sum::<usize>();
        if size > MAX_JOB_OUTPUTS {
            out.push(PlanAction::Warn { leg: key.to_owned(), message: format!("the job's outputs are {size} bytes, over the 1 MiB limit: none were kept") });
            r.outputs.clear();
            r.json_outputs.clear();
        } else if self.output_bytes + size > MAX_RUN_OUTPUTS {
            out.push(PlanAction::Warn { leg: key.to_owned(), message: "the run's outputs are over the 50 MiB limit: this job's were not kept".into() });
            r.outputs.clear();
            r.json_outputs.clear();
        } else {
            self.output_bytes += size;
        }
        r
    }

    /// What follows from a leg ending: `fail-fast`, and a called workflow's caller.
    fn after_leg(&mut self, n: usize, l: usize, c: Conclusion, src: &dyn PlanSource, out: &mut Vec<PlanAction>) {
        let _ = src;
        let Some(leg) = self.leg(n, l) else { return };
        if !c.failed() || leg.spec.continue_on_error {
            return;
        }
        let failed_name = leg.spec.name.clone();
        let failed_combo = leg.combo.clone();
        let Some(node) = self.nodes.get(n) else { return };
        let ff = node.fail_fast.clone();
        if !ff.on {
            return;
        }
        let reason = format!("fail-fast: {failed_name} failed");
        if ff.run {
            for m in 0..self.nodes.len() {
                let count = self.nodes.get(m).map_or(0, |x| x.legs.len());
                for k in 0..count {
                    if (m, k) != (n, l) {
                        self.cancel_leg(m, k, &reason, ff.queued_only, out);
                    }
                }
            }
            return;
        }
        let count = node.legs.len();
        for k in 0..count {
            if k == l {
                continue;
            }
            if let Some(axis) = &ff.axis {
                let shares = self.leg(n, k).is_some_and(|o| matrix::get(&o.combo, axis).zip(matrix::get(&failed_combo, axis)).is_some_and(|(a, b)| matrix::same(a, b)));
                if !shares {
                    continue;
                }
            }
            self.cancel_leg(n, k, &reason, ff.queued_only, out);
        }
    }

    /// Stops a leg that hasn't finished: a waiting one is settled as cancelled, a queued one is
    /// cancelled through the scheduler, and a leg running a called workflow stops its scope.
    fn cancel_leg(&mut self, n: usize, l: usize, reason: &str, queued_only: bool, out: &mut Vec<PlanAction>) {
        let Some(leg) = self.leg(n, l) else { return };
        match leg.state {
            LegState::Waiting => {
                let spec = leg.spec.clone();
                if let Some(leg) = self.leg_mut(n, l) {
                    leg.state = LegState::Done(Conclusion::Cancelled);
                }
                out.push(PlanAction::Settle { leg: Box::new(spec), conclusion: Conclusion::Cancelled, reason: reason.to_owned() });
            }
            LegState::Queued => match leg.called {
                Some(scope) => {
                    let nodes = self.scopes.get(scope).map(|s| s.nodes.clone()).unwrap_or_default();
                    for m in nodes {
                        if self.nodes.get(m).is_some_and(|x| x.state == JobState::Waiting) {
                            if let Some(x) = self.nodes.get_mut(m) {
                                x.state = JobState::Done;
                                x.result = Some(Conclusion::Cancelled);
                            }
                            let id = self.nodes.get(m).map(|x| x.id.clone()).unwrap_or_default();
                            out.push(PlanAction::Job { job: id, conclusion: Conclusion::Cancelled, reason: reason.to_owned() });
                        }
                        let count = self.nodes.get(m).map_or(0, |x| x.legs.len());
                        for k in 0..count {
                            self.cancel_leg(m, k, reason, queued_only, out);
                        }
                    }
                }
                None if !leg.cancel_sent => {
                    let key = leg.spec.key.clone();
                    if let Some(leg) = self.leg_mut(n, l) {
                        leg.cancel_sent = true;
                    }
                    out.push(PlanAction::Cancel { leg: key, reason: reason.to_owned(), queued_only });
                }
                None => {}
            },
            LegState::Done(_) => {}
        }
    }

    /// Moves everything forward that can move: jobs whose needs are done are evaluated and
    /// expanded, legs whose turn has come are queued, finished jobs are concluded, and the run
    /// finishes when its jobs have.
    fn advance(&mut self, src: &dyn PlanSource, out: &mut Vec<PlanAction>) {
        if self.finished.is_some() {
            return;
        }
        loop {
            let mut moved = false;
            for n in 0..self.nodes.len() {
                match self.nodes.get(n).map(|x| x.state) {
                    Some(JobState::Waiting) if self.expandable(n) => {
                        self.expand(n, src, out);
                        moved = true;
                    }
                    Some(JobState::Expanded) => {
                        if self.release(n, src, out) {
                            moved = true;
                        }
                        if self.nodes.get(n).is_some_and(|x| x.legs.iter().all(|l| matches!(l.state, LegState::Done(_)))) {
                            self.finish_job(n, out);
                            moved = true;
                        }
                    }
                    _ => {}
                }
            }
            if !moved {
                break;
            }
        }
        let root: Vec<usize> = self.scopes.first().map(|s| s.nodes.clone()).unwrap_or_default();
        if root.iter().all(|n| self.nodes.get(*n).is_some_and(|x| x.state == JobState::Done)) {
            let results: Vec<Conclusion> = root.iter().filter_map(|n| self.nodes.get(*n).and_then(|x| x.result)).collect();
            let c = if self.cancelled || results.contains(&Conclusion::Cancelled) {
                Conclusion::Cancelled
            } else if results.iter().any(|r| r.failed()) {
                Conclusion::Failure
            } else {
                Conclusion::Success
            };
            self.finished = Some(c);
            out.push(PlanAction::Finish(c));
            return;
        }
        // Nothing in flight and jobs still waiting: they wait on each other (a cycle W02 would
        // have refused) and never will start.
        let in_flight = self.nodes.iter().any(|x| x.legs.iter().any(|l| l.state == LegState::Queued));
        if !in_flight {
            let stuck: Vec<usize> = (0..self.nodes.len()).filter(|n| self.nodes.get(*n).is_some_and(|x| x.state != JobState::Done)).collect();
            for n in stuck {
                let id = self.nodes.get(n).map(|x| x.id.clone()).unwrap_or_default();
                if let Some(x) = self.nodes.get_mut(n) {
                    x.state = JobState::Done;
                    x.result = Some(Conclusion::Failure);
                    for l in &mut x.legs {
                        if !matches!(l.state, LegState::Done(_)) {
                            l.state = LegState::Done(Conclusion::Failure);
                        }
                    }
                }
                out.push(PlanAction::Job { job: id, conclusion: Conclusion::Failure, reason: "its needs can never finish".into() });
            }
            self.advance(src, out);
        }
    }

    fn expandable(&self, n: usize) -> bool {
        let Some(node) = self.nodes.get(n) else { return false };
        node.needs.iter().all(|m| self.nodes.get(*m).is_some_and(|x| x.state == JobState::Done))
            && node.leg_needs.iter().all(|(m, _)| self.nodes.get(*m).is_some_and(|x| x.state != JobState::Waiting))
    }

    /// Every job `n` needs, transitively (within its workflow).
    fn ancestors(&self, n: usize) -> Vec<usize> {
        let mut seen: Vec<usize> = Vec::new();
        let mut stack: Vec<usize> = self.nodes.get(n).map(|x| x.needs.iter().copied().chain(x.leg_needs.iter().map(|(m, _)| *m)).collect()).unwrap_or_default();
        while let Some(m) = stack.pop() {
            if seen.contains(&m) || m == n {
                continue;
            }
            seen.push(m);
            if let Some(x) = self.nodes.get(m) {
                stack.extend(x.needs.iter().copied().chain(x.leg_needs.iter().map(|(k, _)| *k)));
            }
        }
        seen
    }

    /// What the status functions see from a job (W04-R02): its needs, transitively.
    fn status(&self, n: usize) -> StatusView {
        let mut v = StatusView { cancelled: self.cancelled, ..StatusView::default() };
        for m in self.ancestors(n) {
            match self.nodes.get(m).and_then(|x| x.result) {
                Some(c) if c.failed() => v.failed = true,
                Some(Conclusion::Skipped) => v.skipped = true,
                Some(Conclusion::Cancelled) => v.cancelled = true,
                _ => {}
            }
        }
        v
    }

    /// A finished leg's outputs, as consumers in `dialect` see them.
    fn leg_outputs(leg: &Leg, dialect: Dialect) -> Vec<(String, Value)> {
        let mut out: Vec<(String, Value)> = leg.report.outputs.iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| (k.clone(), Value::str(v))).collect();
        for (k, v) in &leg.report.json_outputs {
            let val = match dialect {
                Dialect::Kiln => Value::from_json(v),
                Dialect::Gha => Value::str(&v.to_string()),
            };
            match out.iter_mut().find(|(x, _)| x == k) {
                Some(slot) => slot.1 = val,
                None => out.push((k.clone(), val)),
            }
        }
        out
    }

    /// `needs.<job>` for one needed job, from the given legs: `result`, `outputs` (the leg that
    /// set each last), and kiln's `legs` and `env`.
    fn need_value(&self, m: usize, legs: &[usize], dialect: Dialect) -> Value {
        let Some(node) = self.nodes.get(m) else { return Value::Null };
        let chosen: Vec<&Leg> = legs.iter().filter_map(|l| node.legs.get(*l)).collect();
        let result = if legs.len() == node.legs.len() {
            node.result.unwrap_or(Conclusion::Skipped)
        } else {
            aggregate(chosen.iter().filter_map(|l| if let LegState::Done(c) = l.state { Some(effective(c, l.spec.continue_on_error)) } else { None }))
        };
        let mut ordered: Vec<&Leg> = chosen.clone();
        ordered.sort_by_key(|l| l.finished);
        let mut outputs: Vec<(String, Value)> = Vec::new();
        let mut env: Vec<(String, Value)> = Vec::new();
        for l in &ordered {
            for (k, v) in Self::leg_outputs(l, dialect) {
                match outputs.iter_mut().find(|(x, _)| *x == k) {
                    Some(slot) => slot.1 = v,
                    None => outputs.push((k, v)),
                }
            }
            for (k, v) in &l.report.env {
                match env.iter_mut().find(|(x, _)| x == k) {
                    Some(slot) => slot.1 = Value::str(v),
                    None => env.push((k.clone(), Value::str(v))),
                }
            }
        }
        let mut members = vec![("result".to_owned(), Value::str(result.result())), ("outputs".to_owned(), Value::object(outputs))];
        if dialect == Dialect::Kiln {
            let legs_list: Vec<Value> = chosen
                .iter()
                .map(|l| {
                    Value::object([
                        ("name".to_owned(), Value::str(&l.spec.name)),
                        ("matrix".to_owned(), combo_value(&l.combo)),
                        ("result".to_owned(), Value::str(if let LegState::Done(c) = l.state { c.result() } else { "" })),
                        ("outputs".to_owned(), Value::object(Self::leg_outputs(l, dialect))),
                    ])
                })
                .collect();
            members.push(("legs".to_owned(), Value::array(legs_list)));
            members.push(("env".to_owned(), Value::object(env)));
        }
        Value::object(members)
    }

    /// The `needs` context of a job (all legs of each need), or of one leg with leg-level needs.
    fn needs_context(&self, n: usize, leg_deps: Option<&[(usize, usize)]>) -> Value {
        let Some(node) = self.nodes.get(n) else { return Value::Null };
        let dialect = self.scopes.get(node.scope).map_or(Dialect::Gha, |s| s.dialect);
        let mut members = Vec::new();
        for m in &node.needs {
            let all: Vec<usize> = self.nodes.get(*m).map(|x| (0..x.legs.len()).collect()).unwrap_or_default();
            members.push((self.nodes.get(*m).map(|x| x.local.clone()).unwrap_or_default(), self.need_value(*m, &all, dialect)));
        }
        for (m, _) in &node.leg_needs {
            let legs: Vec<usize> = match leg_deps {
                Some(deps) => deps.iter().filter(|(k, _)| k == m).map(|(_, l)| *l).collect(),
                None => self.nodes.get(*m).map(|x| (0..x.legs.len()).collect()).unwrap_or_default(),
            };
            members.push((self.nodes.get(*m).map(|x| x.local.clone()).unwrap_or_default(), self.need_value(*m, &legs, dialect)));
        }
        Value::object(members)
    }

    fn base_cx(&self, n: usize, needs: Value) -> Cx {
        let inputs = self.scope_of(n).map_or(Value::Null, |s| s.inputs.clone());
        let mut values = vec![("github", self.github.clone()), ("inputs", inputs), ("vars", self.vars.clone()), ("needs", needs)];
        if self.scope_of(n).is_some_and(|s| s.dialect == Dialect::Kiln) {
            values.push(("run", self.run_context(n)));
        }
        Cx { values, status: self.status(n), changed: self.input.changed.clone() }
    }

    /// kiln's `run` context: `run.env` as written by the job's needs, transitively (W04-R21).
    fn run_context(&self, n: usize) -> Value {
        let anc = self.ancestors(n);
        let mut env: Vec<(String, Value)> = Vec::new();
        for (m, k, v) in &self.run_env {
            if anc.contains(m) {
                match env.iter_mut().find(|(x, _)| x == k) {
                    Some(slot) => slot.1 = Value::str(v),
                    None => env.push((k.clone(), Value::str(v))),
                }
            }
        }
        Value::object([("env".to_owned(), Value::object(env))])
    }

    /// A job whose needs are done: its `if`, its matrix, its legs (W04-R02, W04-R07).
    fn expand(&mut self, n: usize, src: &dyn PlanSource, out: &mut Vec<PlanAction>) {
        let Some((job, dialect)) = self.job_def(n).map(|(j, d)| (j.clone(), d)) else { return };
        let id = self.nodes.get(n).map(|x| x.id.clone()).unwrap_or_default();
        let has_leg_needs = self.nodes.get(n).is_some_and(|x| !x.leg_needs.is_empty());
        if !has_leg_needs {
            let cx = self.base_cx(n, self.needs_context(n, None));
            match self.job_if(&job, dialect, &cx) {
                Ok(true) => {}
                Ok(false) => {
                    let reason = skip_reason(&job, cx.status);
                    self.conclude_job(n, Conclusion::Skipped, reason, out);
                    return;
                }
                Err(e) => {
                    self.conclude_job(n, Conclusion::Failure, e, out);
                    return;
                }
            }
        }
        let cx = self.base_cx(n, self.needs_context(n, None));
        let legs = match self.legs_of(n, &job, dialect, &cx, src) {
            Ok(l) => l,
            Err(e) => {
                self.conclude_job(n, Conclusion::Failure, e, out);
                return;
            }
        };
        let (ff, max_parallel, per_axis) = match strategy_settings(&job, dialect, &cx) {
            Ok(s) => s,
            Err(e) => {
                self.conclude_job(n, Conclusion::Failure, e, out);
                return;
            }
        };
        if legs.is_empty() {
            self.conclude_job(n, Conclusion::Skipped, "the matrix has no combinations left".into(), out);
            return;
        }
        let total = legs.len();
        let job_name_tpl = job.name.clone();
        let names_matrix = job_name_tpl.as_ref().is_some_and(|t| Template::parse(&t.src, dialect).is_ok_and(|p| p.references().iter().any(|r| r.root().eq_ignore_ascii_case("matrix"))));
        let mut built: Vec<Leg> = Vec::new();
        let mut failures: Vec<(LegSpec, String)> = Vec::new();
        for (i, (combo, items, requires, emulated)) in legs.into_iter().enumerate() {
            let strategy = Value::object([
                ("fail-fast".to_owned(), Value::Bool(ff.on)),
                ("job-index".to_owned(), Value::Number(i as f64)),
                ("job-total".to_owned(), Value::Number(total as f64)),
                ("max-parallel".to_owned(), Value::Number(if max_parallel == 0 { total } else { max_parallel } as f64)),
            ]);
            let mut lcx = self.base_cx(n, self.needs_context(n, None));
            lcx.values.push(("strategy", strategy.clone()));
            lcx.values.push(("matrix", combo_value(&combo)));
            let base_name = match &job_name_tpl {
                Some(t) => template(t, dialect, &lcx, "name").unwrap_or_else(|_| t.src.clone()),
                None => job.id.clone(),
            };
            let display = if names_matrix || (combo.is_empty() && items.is_empty()) { base_name.clone() } else { matrix::leg_name(&base_name, &combo) };
            let prefix = self.scope_of(n).map(|s| s.prefix.clone()).unwrap_or_default();
            let key = format!("{prefix}{}", matrix::leg_name(&job.id, &combo));
            let key = self.unique_key(key);
            let spec = self.leg_spec(n, &job, dialect, &lcx, &combo, strategy, key.clone(), base_name, format!("{prefix}{display}"), items, requires, emulated);
            match spec {
                Ok(spec) => built.push(Leg { spec, combo, state: LegState::Waiting, report: LegReport::default(), finished: 0, called: None, deps: Vec::new(), cancel_sent: false }),
                Err((spec, e)) => failures.push((*spec, e)),
            }
        }
        // Leg-level needs (W04-R17).
        let leg_needs = self.nodes.get(n).map(|x| x.leg_needs.clone()).unwrap_or_default();
        for leg in &mut built {
            let mut deps = Vec::new();
            for (m, how) in &leg_needs {
                let Some(other) = self.nodes.get(*m) else { continue };
                for (k, ol) in other.legs.iter().enumerate() {
                    let pick = match how {
                        LegNeed::Same => leg.combo.iter().all(|(key, v)| matrix::get(&ol.combo, key).is_none_or(|w| matrix::same(v, w))),
                        LegNeed::Where(t) => {
                            let mut wcx = self.base_cx(n, Value::Null);
                            wcx.values.push(("matrix", combo_value(&ol.combo)));
                            condition(t, dialect, &wcx, "needs.where").unwrap_or(false)
                        }
                    };
                    if pick {
                        deps.push((*m, k));
                    }
                }
            }
            leg.deps = deps;
        }
        let start = self.nodes.get(n).map_or(0, |x| x.legs.len());
        if let Some(node) = self.nodes.get_mut(n) {
            node.state = JobState::Expanded;
            node.fail_fast = ff;
            node.max_parallel = max_parallel;
            node.per_axis = per_axis;
            for (i, leg) in built.into_iter().enumerate() {
                self.keys.insert(leg.spec.key.clone(), (n, start + i));
                node.legs.push(leg);
            }
        }
        for (spec, e) in failures {
            let at = self.nodes.get(n).map_or(0, |x| x.legs.len());
            self.keys.insert(spec.key.clone(), (n, at));
            if let Some(node) = self.nodes.get_mut(n) {
                node.legs.push(Leg { spec: spec.clone(), combo: Vec::new(), state: LegState::Done(Conclusion::Failure), report: LegReport::default(), finished: 0, called: None, deps: Vec::new(), cancel_sent: false });
            }
            out.push(PlanAction::Settle { leg: Box::new(spec), conclusion: Conclusion::Failure, reason: e });
        }
        let _ = id;
    }

    fn unique_key(&self, key: String) -> String {
        if !self.keys.contains_key(&key) {
            return key;
        }
        (2..).map(|i| format!("{key} #{i}")).find(|k| !self.keys.contains_key(k)).unwrap_or(key)
    }

    fn job_if(&self, job: &Job, dialect: Dialect, cx: &Cx) -> Result<bool, String> {
        match &job.cond {
            Some(t) => condition(t, dialect, cx, "if"),
            None => {
                let s = cx.status;
                Ok(!(s.failed || s.cancelled || s.skipped))
            }
        }
    }

    /// The job's combinations, each with its shard items, requirements and whether it may be
    /// emulated (W04-R07, R11, R13 to R15).
    #[allow(clippy::type_complexity)]
    fn legs_of(&mut self, n: usize, job: &Job, dialect: Dialect, cx: &Cx, src: &dyn PlanSource) -> Result<Vec<(Combo, Vec<String>, Vec<String>, bool)>, String> {
        let limit = if dialect == Dialect::Gha { 256 } else { self.settings.max_legs };
        let strategy = job.strategy.as_ref();
        let what = "strategy";
        let mut excluded: Vec<(Combo, usize)> = Vec::new();
        let (mut combos, filter, platforms, emulate): (Vec<Combo>, Option<Tpl>, bool, Vec<String>) = match strategy.and_then(|s| s.matrix.as_ref()) {
            None => (vec![Vec::new()], None, false, Vec::new()),
            Some(Matrix::Expr(t)) => {
                let v = template_value(t, dialect, cx, what)?;
                let Value::Object(m) = v else { return Err("strategy.matrix must evaluate to a mapping".into()) };
                let mut axes = Vec::new();
                let (mut inc, mut exc) = (Vec::new(), Vec::new());
                for (k, v) in m.0.iter() {
                    let items = v.items().map(<[Value]>::to_vec).ok_or_else(|| format!("matrix variable `{k}` must be a list of values"))?;
                    if k.eq_ignore_ascii_case("include") {
                        inc = items;
                    } else if k.eq_ignore_ascii_case("exclude") {
                        exc = items;
                    } else {
                        axes.push((k.to_string(), items));
                    }
                }
                let (combos, gone) = matrix::expand_all(&axes, &inc, &exc, limit)?;
                excluded = gone;
                (combos, None, false, Vec::new())
            }
            Some(Matrix::Spec(m)) => {
                let mut axes: Vec<(String, Vec<Value>)> = Vec::new();
                if let Some(p) = &m.platforms {
                    axes.push(("platform".into(), axis_values(p, dialect, cx, "platforms")?));
                }
                for (k, a) in &m.axes {
                    axes.push((k.clone(), axis_values(a, dialect, cx, k)?));
                }
                let list = |v: &Option<YVal>, w: &str| -> Result<Vec<Value>, String> {
                    match v {
                        None => Ok(Vec::new()),
                        Some(x) => {
                            let val = yval(x, dialect, cx, w)?;
                            val.items().map(<[Value]>::to_vec).ok_or_else(|| format!("`{w}` must be a list of mappings"))
                        }
                    }
                };
                let (inc, exc) = (list(&m.include, "include")?, list(&m.exclude, "exclude")?);
                let (combos, gone) = matrix::expand_all(&axes, &inc, &exc, limit)?;
                excluded = gone;
                (combos, m.filter.clone(), m.platforms.is_some(), m.emulate.clone())
            }
        };
        if !excluded.is_empty() {
            let id = self.nodes.get(n).map(|x| x.id.clone()).unwrap_or_default();
            let list = self.removed.entry(id).or_default();
            for (c, i) in excluded {
                list.push((json(&combo_value(&c)), format!("removed by exclude entry {}", i + 1)));
            }
        }
        // `where:` (W04-R11).
        if let Some(f) = &filter {
            let mut kept = Vec::new();
            for c in combos {
                let mut wcx = Cx { values: cx.values.clone(), status: cx.status, changed: cx.changed.clone() };
                wcx.values.push(("matrix", combo_value(&c)));
                if condition(f, dialect, &wcx, "strategy.matrix.where")? {
                    kept.push(c);
                } else {
                    let id = self.nodes.get(n).map(|x| x.id.clone()).unwrap_or_default();
                    self.removed.entry(id).or_default().push((json(&combo_value(&c)), "removed by where".into()));
                }
            }
            combos = kept;
        }
        // Platforms (W04-R13): `matrix.os` and `matrix.arch` next to `matrix.platform`, and the
        // requirement to run natively unless the platform may be emulated.
        let mut out = Vec::new();
        for mut c in combos {
            let mut requires = Vec::new();
            let mut emulated = false;
            if platforms {
                let p = matrix::get(&c, "platform").map(Value::coerce_string).unwrap_or_default();
                let plat = matrix::Platform::parse(&p)?;
                if c.iter().any(|(k, _)| k.eq_ignore_ascii_case("os") || k.eq_ignore_ascii_case("arch")) {
                    return Err("a matrix with `platforms` gets `os` and `arch` from them: drop its own `os` and `arch`".into());
                }
                let at = c.iter().position(|(k, _)| k == "platform").map_or(0, |i| i + 1);
                c.insert(at, ("arch".into(), Value::str(&plat.arch)));
                c.insert(at, ("os".into(), Value::str(&plat.os)));
                emulated = emulate.contains(&p);
                requires.push(if emulated { plat.emulated_requirement(&p) } else { plat.requirement() });
            }
            out.push((c, Vec::new(), requires, emulated));
        }
        // `shard:` and `chunk:` (W04-R14, R15): each combination split over work items.
        if let Some(s) = strategy.and_then(|s| s.shard.as_ref()) {
            let items = work_items(s, dialect, cx, src)?;
            let job_id = self.nodes.get(n).map(|x| x.id.clone()).unwrap_or_default();
            let durations = src.durations(&job_id, &items);
            let get = |k: &str| s.get(k);
            let by_timing = get("balance").and_then(YVal::as_str).is_none_or(|b| b == "timing");
            let count = match get("count") {
                Some(YVal::Num(c)) if *c >= 1.0 => *c as usize,
                Some(YVal::Str(t)) if t.src != "auto" => number(&template_value(t, dialect, cx, "shard.count")?).filter(|c| *c >= 1.0).map_or(1, |c| c as usize),
                _ => {
                    let target = get("target").and_then(YVal::as_str).and_then(seconds).unwrap_or(600.0);
                    let known: Vec<f64> = durations.iter().flatten().copied().collect();
                    let median = if known.is_empty() { 1.0 } else { known.iter().sum::<f64>() / known.len() as f64 };
                    let total: f64 = durations.iter().map(|d| d.unwrap_or(median)).sum();
                    matrix::shard_count(total, target)
                }
            };
            let split = matrix::balance(&items, &durations, count.min(items.len().max(1)), by_timing);
            out = shard_legs(out, &split, limit)?;
        } else if let Some(c) = strategy.and_then(|s| s.chunk.as_ref()) {
            let items = work_items(c, dialect, cx, src)?;
            let size = match c.get("size") {
                Some(YVal::Num(s)) if *s >= 1.0 => *s as usize,
                _ => return Err("`chunk.size` must be a number of items, at least 1".into()),
            };
            let split: Vec<Vec<String>> = items.chunks(size).map(<[String]>::to_vec).collect();
            out = shard_legs(out, &split, limit)?;
        }
        Ok(out)
    }

    /// Everything about one leg that is decided before it runs.
    #[allow(clippy::too_many_arguments)]
    fn leg_spec(&self, n: usize, job: &Job, dialect: Dialect, cx: &Cx, combo: &Combo, strategy: Value, key: String, job_name: String, name: String, items: Vec<String>, mut requires: Vec<String>, emulated: bool) -> Result<LegSpec, (Box<LegSpec>, String)> {
        let scope = self.scope_of(n);
        let wf = scope.map(|s| Arc::clone(&s.wf)).unwrap_or_default();
        let job_index = self.nodes.get(n).map_or(0, |x| x.index);
        let id = self.nodes.get(n).map(|x| x.id.clone()).unwrap_or_default();
        let needs = json(&self.needs_context(n, None));
        let mut spec = LegSpec {
            key,
            job: id,
            job_name,
            name,
            matrix: json(&combo_value(combo)),
            strategy: json(&strategy),
            needs,
            inputs: json(&scope.map_or(Value::Null, |s| s.inputs.clone())),
            runs_on: RunsOnSpec::default(),
            requires: Vec::new(),
            timeout_minutes: if dialect == Dialect::Kiln { 60 } else { 360 },
            continue_on_error: false,
            environment: None,
            concurrency: None,
            permissions: perms::Resolved::default(),
            container: None,
            services: Vec::new(),
            secrets: scope.map_or(SecretsSpec::Run, |s| s.secrets.clone()),
            secret_outputs: Vec::new(),
            env_from: Vec::new(),
            run_env: Vec::new(),
            items,
            emulated,
            reads: reads(job),
            workflow: wf.clone(),
            job_index,
        };
        let fail = |spec: &LegSpec, e: String| (Box::new(spec.clone()), e);
        // runs-on (a job that calls a workflow has none).
        spec.runs_on = match &job.runs_on {
            Some(RunsOn::Labels(ls)) => {
                let mut labels = Vec::new();
                for l in ls {
                    match template_value(l, dialect, cx, "runs-on") {
                        Ok(v) => match v.items() {
                            Some(list) => labels.extend(list.iter().map(Value::coerce_string)),
                            None => labels.push(v.coerce_string()),
                        },
                        Err(e) => return Err(fail(&spec, e)),
                    }
                }
                RunsOnSpec { labels, group: None }
            }
            Some(RunsOn::Group { group, labels }) => {
                let g = match group {
                    Some(t) => Some(template(t, dialect, cx, "runs-on.group").map_err(|e| fail(&spec, e))?),
                    None => None,
                };
                let ls = labels.iter().map(|l| template(l, dialect, cx, "runs-on.labels")).collect::<Result<Vec<_>, _>>().map_err(|e| fail(&spec, e))?;
                RunsOnSpec { labels: ls, group: g }
            }
            None if job.uses.is_none() && dialect == Dialect::Kiln => RunsOnSpec { labels: self.settings.default_runs_on.clone(), group: None },
            None => RunsOnSpec::default(),
        };
        for t in &job.kiln.requires {
            requires.push(template(t, dialect, cx, "requires").map_err(|e| fail(&spec, e))?);
        }
        spec.requires = requires;
        if let Some(t) = &job.timeout_minutes {
            let v = yval(t, dialect, cx, "timeout-minutes").map_err(|e| fail(&spec, e))?;
            let m = number(&v).ok_or_else(|| fail(&spec, "timeout-minutes must be a number".into()))?;
            spec.timeout_minutes = if m < 1.0 { 1 } else { m.min(f64::from(u32::MAX)) as u32 };
        }
        spec.timeout_minutes = spec.timeout_minutes.min(self.settings.max_job_minutes.max(1));
        if let Some(c) = &job.continue_on_error {
            spec.continue_on_error = truthy(&yval(c, dialect, cx, "continue-on-error").map_err(|e| fail(&spec, e))?);
        }
        if let Some(e) = &job.environment {
            let name = template(&e.name, dialect, cx, "environment").map_err(|e| fail(&spec, e))?;
            let deployment = match &e.deployment {
                Some(d) => truthy(&yval(d, dialect, cx, "environment.deployment").map_err(|e| fail(&spec, e))?),
                None => true,
            };
            spec.environment = Some(EnvironmentSpec { name, url: e.url.as_ref().map(|u| u.src.clone()), deployment });
        }
        if let Some(c) = &job.concurrency {
            spec.concurrency = Some(group(c, dialect, cx, "concurrency").map_err(|e| fail(&spec, e))?);
        }
        let own = perms::resolve(job.permissions.as_ref(), scope.and_then(|s| s.wf.permissions.as_ref()), self.settings.default_permissions, self.input.trusted);
        spec.permissions = match scope.and_then(|s| s.perms_cap.as_ref()) {
            Some(cap) => own.min(cap),
            None => own,
        };
        if let Some(c) = &job.container {
            spec.container = Some(template(&c.image, dialect, cx, "container.image").map_err(|e| fail(&spec, e))?);
        }
        for (sid, c) in &job.services {
            spec.services.push((sid.clone(), template(&c.image, dialect, cx, "services.image").map_err(|e| fail(&spec, e))?));
        }
        // Secret outputs of direct needs, and `env-from` (W04-R21, R22).
        if let Some(node) = self.nodes.get(n) {
            for m in node.needs.iter().chain(node.leg_needs.iter().map(|(m, _)| m)) {
                let Some(other) = self.nodes.get(*m) else { continue };
                for l in &other.legs {
                    for s in &l.report.secret_outputs {
                        spec.secret_outputs.push(SecretOutput { job: other.local.clone(), name: s.clone(), leg: l.spec.key.clone() });
                    }
                }
                if job.kiln.env_from.contains(&other.local) {
                    let mut legs: Vec<&Leg> = other.legs.iter().collect();
                    legs.sort_by_key(|l| l.finished);
                    for l in legs {
                        for (k, v) in &l.report.env {
                            match spec.env_from.iter_mut().find(|(x, _)| x == k) {
                                Some(slot) => slot.1 = v.clone(),
                                None => spec.env_from.push((k.clone(), v.clone())),
                            }
                        }
                    }
                }
            }
        }
        let anc = self.ancestors(n);
        for (m, k, v) in &self.run_env {
            if anc.contains(m) {
                match spec.run_env.iter_mut().find(|(x, _)| x == k) {
                    Some(slot) => slot.1 = v.clone(),
                    None => spec.run_env.push((k.clone(), v.clone())),
                }
            }
        }
        Ok(spec)
    }

    /// Queues the job's legs whose turn has come: their needs done (leg-level), under
    /// `max-parallel`, past `when:` and a per-leg `if:`. Returns whether anything changed.
    fn release(&mut self, n: usize, src: &dyn PlanSource, out: &mut Vec<PlanAction>) -> bool {
        let Some((job, dialect)) = self.job_def(n).map(|(j, d)| (j.clone(), d)) else { return false };
        let Some(node) = self.nodes.get(n) else { return false };
        let (limit, per_axis, count) = (node.max_parallel, node.per_axis.clone(), node.legs.len());
        let has_leg_needs = !node.leg_needs.is_empty();
        let mut changed = false;
        for l in 0..count {
            let Some(leg) = self.leg(n, l) else { continue };
            if leg.state != LegState::Waiting {
                continue;
            }
            if !leg.deps.iter().all(|(m, k)| matches!(self.leg(*m, *k).map(|x| x.state), Some(LegState::Done(_)))) {
                continue;
            }
            let running: Vec<&Leg> = self.nodes.get(n).map(|x| x.legs.iter().filter(|o| o.state == LegState::Queued).collect()).unwrap_or_default();
            if limit > 0 && running.len() >= limit {
                break;
            }
            let combo = leg.combo.clone();
            let over_axis = per_axis.iter().any(|(axis, value, max)| {
                matrix::get(&combo, axis).is_some_and(|v| matrix::display(v) == *value) && running.iter().filter(|o| matrix::get(&o.combo, axis).is_some_and(|v| matrix::display(v) == *value)).count() >= *max
            });
            if over_axis {
                continue;
            }
            let deps = leg.deps.clone();
            let mut spec = leg.spec.clone();
            // A leg with leg-level needs sees only its own needed legs, and decides `if` itself.
            if has_leg_needs {
                spec.needs = json(&self.needs_context(n, Some(&deps)));
                let mut cx = self.base_cx(n, self.needs_context(n, Some(&deps)));
                cx.status = self.leg_status(n, &deps);
                match self.job_if(&job, dialect, &cx) {
                    Ok(true) => {}
                    Ok(false) => {
                        let reason = skip_reason(&job, cx.status);
                        self.settle(n, l, spec, Conclusion::Skipped, reason, out);
                        changed = true;
                        continue;
                    }
                    Err(e) => {
                        self.settle(n, l, spec, Conclusion::Failure, e, out);
                        changed = true;
                        continue;
                    }
                }
            }
            // `when:` (W04-R12).
            if let Some(w) = job.strategy.as_ref().and_then(|s| s.when.as_ref()) {
                let mut cx = self.base_cx(n, Value::from_json(&spec.needs));
                cx.values.push(("strategy", Value::from_json(&spec.strategy)));
                cx.values.push(("matrix", combo_value(&combo)));
                match condition(w, dialect, &cx, "strategy.when") {
                    Ok(true) => {}
                    Ok(false) => {
                        self.settle(n, l, spec, Conclusion::Skipped, "skipped by when".into(), out);
                        changed = true;
                        continue;
                    }
                    Err(e) => {
                        self.settle(n, l, spec, Conclusion::Failure, e, out);
                        changed = true;
                        continue;
                    }
                }
            }
            if job.uses.is_some() {
                match self.call(n, l, &job, dialect, src) {
                    Ok(()) => {}
                    Err(e) => {
                        self.settle(n, l, spec, Conclusion::Failure, e, out);
                        changed = true;
                        continue;
                    }
                }
                if let Some(leg) = self.leg_mut(n, l) {
                    leg.state = LegState::Queued;
                }
            } else {
                if let Some(leg) = self.leg_mut(n, l) {
                    leg.state = LegState::Queued;
                    leg.spec = spec.clone();
                }
                out.push(PlanAction::Queue(Box::new(spec)));
            }
            changed = true;
        }
        changed
    }

    fn leg_status(&self, n: usize, deps: &[(usize, usize)]) -> StatusView {
        let mut v = self.status(n);
        // The status of leg-level needs is that of the legs needed, not of their whole jobs.
        let leg_jobs: Vec<usize> = self.nodes.get(n).map(|x| x.leg_needs.iter().map(|(m, _)| *m).collect()).unwrap_or_default();
        let others: Vec<usize> = self.ancestors(n).into_iter().filter(|m| !leg_jobs.contains(m)).collect();
        v.failed = others.iter().any(|m| self.nodes.get(*m).and_then(|x| x.result).is_some_and(Conclusion::failed));
        v.skipped = others.iter().any(|m| self.nodes.get(*m).and_then(|x| x.result) == Some(Conclusion::Skipped));
        for (m, k) in deps {
            if let Some(leg) = self.leg(*m, *k)
                && let LegState::Done(c) = leg.state
            {
                match effective(c, leg.spec.continue_on_error) {
                    c if c.failed() => v.failed = true,
                    Conclusion::Skipped => v.skipped = true,
                    Conclusion::Cancelled => v.cancelled = true,
                    _ => {}
                }
            }
        }
        if deps.is_empty() && !leg_jobs.is_empty() {
            // The needed legs don't exist (the needed job was skipped, or chose none).
            v.skipped = true;
        }
        v
    }

    fn settle(&mut self, n: usize, l: usize, spec: LegSpec, c: Conclusion, reason: String, out: &mut Vec<PlanAction>) {
        self.seq += 1;
        let seq = self.seq;
        if let Some(leg) = self.leg_mut(n, l) {
            leg.state = LegState::Done(c);
            leg.finished = seq;
        }
        out.push(PlanAction::Settle { leg: Box::new(spec), conclusion: c, reason });
    }

    /// A leg of a job that calls a reusable workflow: the workflow's jobs join the plan as a
    /// scope of their own (W04-R24).
    fn call(&mut self, n: usize, l: usize, job: &Job, dialect: Dialect, src: &dyn PlanSource) -> Result<(), String> {
        let uses = job.uses.clone().unwrap_or_default();
        let depth = self.scope_of(n).map_or(0, |s| s.depth) + 1;
        if depth > MAX_CALL_DEPTH {
            return Err(format!("reusable workflows nest at most {MAX_CALL_DEPTH} deep"));
        }
        if !self.called.contains(&uses) {
            if self.called.len() >= MAX_CALLED_WORKFLOWS {
                return Err(format!("a run calls at most {MAX_CALLED_WORKFLOWS} reusable workflows"));
            }
            self.called.push(uses.clone());
        }
        let wf = src.workflow(&uses)?;
        let call = wf.trigger("workflow_call").ok_or_else(|| format!("`{uses}` is not a reusable workflow: it has no `on.workflow_call`"))?.clone();
        let Some(leg) = self.leg(n, l) else { return Err("no such leg".into()) };
        let combo = leg.combo.clone();
        let strategy = Value::from_json(&leg.spec.strategy);
        let leg_name = leg.spec.name.clone();
        let leg_perms = leg.spec.permissions.clone();
        let mut cx = self.base_cx(n, self.needs_context(n, None));
        cx.values.push(("strategy", strategy));
        cx.values.push(("matrix", combo_value(&combo)));
        // Inputs: each defined one from `with`, or its default; `with` may not name others.
        let mut with: Vec<(String, Value)> = Vec::new();
        for (k, v) in &job.with {
            with.push((k.clone(), yval(v, dialect, &cx, &format!("with.{k}"))?));
        }
        for (k, _) in &with {
            if !call.inputs.iter().any(|i| i.name.eq_ignore_ascii_case(k)) {
                return Err(format!("`with.{k}`: `{uses}` has no input `{k}`"));
            }
        }
        let called_dialect = wf.dialect.unwrap_or(Dialect::Gha);
        let dcx = Cx { values: vec![("github", self.github.clone()), ("inputs", Value::Null), ("vars", self.vars.clone())], status: StatusView::default(), changed: None };
        let mut inputs: Vec<(String, Value)> = Vec::new();
        for i in &call.inputs {
            let given = with.iter().find(|(k, _)| k.eq_ignore_ascii_case(&i.name)).map(|(_, v)| v.clone());
            let v = match given {
                Some(v) => v,
                None => match &i.default {
                    Some(d) => yval(d, called_dialect, &dcx, &format!("inputs.{}.default", i.name))?,
                    None if i.required => return Err(format!("`{uses}` requires the input `{}`", i.name)),
                    None => match i.ty.as_deref() {
                        Some("boolean") => Value::Bool(false),
                        Some("number") => Value::Number(0.0),
                        _ => Value::str(""),
                    },
                },
            };
            let v = match i.ty.as_deref() {
                Some("boolean") => match &v {
                    Value::Bool(_) => v,
                    Value::String(s) if s.eq_ignore_ascii_case("true") || s.eq_ignore_ascii_case("false") => Value::Bool(s.eq_ignore_ascii_case("true")),
                    _ => return Err(format!("the input `{}` of `{uses}` is a boolean, not {}", i.name, matrix::display(&v))),
                },
                Some("number") => Value::Number(number(&v).ok_or_else(|| format!("the input `{}` of `{uses}` is a number, not `{}`", i.name, matrix::display(&v)))?),
                _ => Value::str(&v.coerce_string()),
            };
            inputs.push((i.name.clone(), v));
        }
        // Secrets: inherited, mapped, or none; required ones must be passed.
        let caller_secrets = self.scope_of(n).map_or(SecretsSpec::Run, |s| s.secrets.clone());
        let secrets = match &job.secrets {
            Some(JobSecrets::Inherit) => caller_secrets,
            Some(JobSecrets::Map(m)) => {
                for d in call.secrets.iter().filter(|d| d.required) {
                    if !m.iter().any(|(k, _)| k.eq_ignore_ascii_case(&d.name)) {
                        return Err(format!("`{uses}` requires the secret `{}`", d.name));
                    }
                }
                SecretsSpec::Mapped { map: m.iter().map(|(k, t)| (k.clone(), t.src.clone())).collect(), from: Box::new(caller_secrets) }
            }
            None => {
                if let Some(d) = call.secrets.iter().find(|d| d.required) {
                    return Err(format!("`{uses}` requires the secret `{}`", d.name));
                }
                SecretsSpec::None
            }
        };
        let id_prefix = format!("{}{}/", self.scope_of(n).map(|s| s.id_prefix.clone()).unwrap_or_default(), self.nodes.get(n).map(|x| x.local.clone()).unwrap_or_default());
        let scope = self.add_scope(wf, called_dialect, Value::object(inputs), format!("{leg_name} / "), id_prefix, Some((n, l)), depth, Some(leg_perms), secrets);
        if let Some(leg) = self.leg_mut(n, l) {
            leg.called = Some(scope);
        }
        Ok(())
    }

    /// A job whose legs are all done: its result, and its caller's leg when it was the last of a
    /// called workflow.
    fn finish_job(&mut self, n: usize, out: &mut Vec<PlanAction>) {
        let Some(node) = self.nodes.get(n) else { return };
        let result = if node.legs.is_empty() {
            Conclusion::Skipped
        } else {
            aggregate(node.legs.iter().filter_map(|l| if let LegState::Done(c) = l.state { Some(effective(c, l.spec.continue_on_error)) } else { None }))
        };
        if let Some(node) = self.nodes.get_mut(n) {
            node.state = JobState::Done;
            node.result = Some(result);
        }
        self.after_job(n, out);
    }

    /// A job ended without legs.
    fn conclude_job(&mut self, n: usize, c: Conclusion, reason: String, out: &mut Vec<PlanAction>) {
        let id = self.nodes.get(n).map(|x| x.id.clone()).unwrap_or_default();
        if let Some(node) = self.nodes.get_mut(n) {
            node.state = JobState::Done;
            node.result = Some(c);
        }
        out.push(PlanAction::Job { job: id, conclusion: c, reason });
        self.after_job(n, out);
    }

    /// When the last job of a called workflow ends, its caller's leg ends, with the workflow's
    /// `on.workflow_call.outputs` (W04-R24).
    fn after_job(&mut self, n: usize, out: &mut Vec<PlanAction>) {
        let Some(scope_ix) = self.nodes.get(n).map(|x| x.scope) else { return };
        let Some(scope) = self.scopes.get(scope_ix) else { return };
        let Some((cn, cl)) = scope.caller else { return };
        if !scope.nodes.iter().all(|m| self.nodes.get(*m).is_some_and(|x| x.state == JobState::Done)) {
            return;
        }
        let results: Vec<Conclusion> = scope.nodes.iter().filter_map(|m| self.nodes.get(*m).and_then(|x| x.result)).collect();
        let conclusion = if results.iter().any(|r| r.failed()) {
            Conclusion::Failure
        } else if results.contains(&Conclusion::Cancelled) {
            Conclusion::Cancelled
        } else {
            Conclusion::Success
        };
        let jobs = Value::object(scope.nodes.iter().filter_map(|m| {
            let x = self.nodes.get(*m)?;
            let all: Vec<usize> = (0..x.legs.len()).collect();
            Some((x.local.clone(), self.need_value(*m, &all, scope.dialect)))
        }));
        let cx = Cx { values: vec![("github", self.github.clone()), ("jobs", jobs), ("vars", self.vars.clone()), ("inputs", scope.inputs.clone())], status: StatusView::default(), changed: None };
        let mut outputs = Vec::new();
        if let Some(call) = scope.wf.trigger("workflow_call") {
            for o in &call.outputs {
                match template(&o.value, scope.dialect, &cx, &format!("on.workflow_call.outputs.{}", o.name)) {
                    Ok(v) => outputs.push((o.name.clone(), v)),
                    Err(e) => out.push(PlanAction::Warn { leg: self.leg(cn, cl).map(|x| x.spec.key.clone()).unwrap_or_default(), message: e }),
                }
            }
        }
        self.seq += 1;
        let seq = self.seq;
        if let Some(leg) = self.leg_mut(cn, cl) {
            leg.state = LegState::Done(conclusion);
            leg.report = LegReport { conclusion: Some(conclusion), outputs, ..LegReport::default() };
            leg.finished = seq;
        }
        let _ = out;
    }

    /// The plan without running it (W04-R29): every leg the run would queue if each succeeded
    /// with no outputs, what the plan decided along the way, and what `exclude` and `where:`
    /// removed. Jobs
    /// whose `if` or matrix read needs are marked: what a real run decides for them may differ.
    pub fn preview(wf: Arc<Workflow>, input: RunInput, settings: Settings, src: &dyn PlanSource) -> Result<Preview, String> {
        let reads_needs: Vec<String> = wf
            .jobs
            .iter()
            .filter(|j| {
                let mut srcs: Vec<&str> = j.cond.iter().map(|t| t.src.as_str()).collect();
                if let Some(Matrix::Expr(t)) = j.strategy.as_ref().and_then(|s| s.matrix.as_ref()) {
                    srcs.push(&t.src);
                }
                srcs.iter().any(|s| s.contains("needs."))
            })
            .map(|j| j.id.clone())
            .collect();
        let (mut plan, mut actions) = RunPlan::start(wf, input, settings, src)?;
        let mut legs = Vec::new();
        let mut decided = Vec::new();
        let mut guard = 0usize;
        while let Some(a) = actions.pop() {
            guard += 1;
            if guard > 100_000 {
                return Err("the preview did not settle".into());
            }
            match a {
                PlanAction::Queue(spec) => {
                    let key = spec.key.clone();
                    legs.push(*spec);
                    actions.extend(plan.on_leg_finished(&key, LegReport::of(Conclusion::Success), src));
                }
                PlanAction::Settle { leg, conclusion, reason } => decided.push(Decision { job: leg.job.clone(), leg: Some(leg.key.clone()), conclusion, reason }),
                PlanAction::Job { job, conclusion, reason } => decided.push(Decision { job, leg: None, conclusion, reason }),
                PlanAction::Cancel { .. } | PlanAction::Warn { .. } | PlanAction::Finish(_) => {}
            }
        }
        legs.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(Preview { name: plan.name.clone(), legs, decided, removed: plan.removed.clone(), later: reads_needs })
    }
}

/// What [`RunPlan::preview`] shows.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Preview {
    pub name: Option<String>,
    pub legs: Vec<LegSpec>,
    pub decided: Vec<Decision>,
    pub removed: BTreeMap<String, Vec<(serde_json::Value, String)>>,
    /// Jobs whose `if` or matrix read their needs, decided only when those finish.
    pub later: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Decision {
    pub job: String,
    pub leg: Option<String>,
    pub conclusion: Conclusion,
    pub reason: String,
}

/// A leg's conclusion as its job and the run count it: a failure under `continue-on-error`
/// doesn't fail them (GitHub-exact).
fn effective(c: Conclusion, continue_on_error: bool) -> Conclusion {
    if continue_on_error && c.failed() { Conclusion::Success } else { c }
}

/// A job's result from its legs': failure if any failed, else cancelled if any was, else skipped
/// if all were, else success.
fn aggregate(cs: impl Iterator<Item = Conclusion>) -> Conclusion {
    let all: Vec<Conclusion> = cs.collect();
    if all.iter().any(|c| c.failed()) {
        Conclusion::Failure
    } else if all.contains(&Conclusion::Cancelled) {
        Conclusion::Cancelled
    } else if all.iter().all(|c| *c == Conclusion::Skipped) {
        Conclusion::Skipped
    } else {
        Conclusion::Success
    }
}

fn skip_reason(job: &Job, s: StatusView) -> String {
    match &job.cond {
        Some(t) => format!("skipped: `if: {}` was false", t.src),
        None if s.cancelled => "skipped: the run was cancelled".into(),
        None if s.failed => "skipped: a job it needs failed".into(),
        None => "skipped: a job it needs was skipped".into(),
    }
}

fn axis_values(a: &Axis, dialect: Dialect, cx: &Cx, name: &str) -> Result<Vec<Value>, String> {
    let what = format!("strategy.matrix.{name}");
    match a {
        Axis::Expr(t) => {
            let v = template_value(t, dialect, cx, &what)?;
            v.items().map(<[Value]>::to_vec).ok_or_else(|| format!("{what}: matrix variable `{name}` must be a list of values"))
        }
        Axis::Values(vs) => vs.iter().map(|v| yval(v, dialect, cx, &what)).collect(),
    }
}

/// `fail-fast` and `max-parallel` (W04-R09, R16).
#[allow(clippy::type_complexity)]
fn strategy_settings(job: &Job, dialect: Dialect, cx: &Cx) -> Result<(FailFast, usize, Vec<(String, String, usize)>), String> {
    let Some(s) = &job.strategy else { return Ok((FailFast { on: true, ..FailFast::default() }, 0, Vec::new())) };
    let mut ff = FailFast { on: true, ..FailFast::default() };
    match &s.fail_fast {
        Some(YVal::Map(m)) => {
            let get = |k: &str| m.iter().find(|(x, _)| x == k).and_then(|(_, v)| v.as_str()).map(str::to_owned);
            match get("scope").as_deref() {
                Some("run") => ff.run = true,
                Some(a) if a.starts_with("axis:") => ff.axis = a.strip_prefix("axis:").map(str::to_owned),
                _ => {}
            }
            ff.queued_only = get("cancel").as_deref() == Some("queued");
        }
        Some(v) => ff.on = truthy(&yval(v, dialect, cx, "strategy.fail-fast")?),
        None => {}
    }
    let mut max = 0usize;
    let mut per = Vec::new();
    match &s.max_parallel {
        Some(YVal::Map(m)) => {
            if let Some((_, t)) = m.iter().find(|(k, _)| k == "total") {
                max = number(&yval(t, dialect, cx, "strategy.max-parallel.total")?).map_or(0, |n| n.max(1.0) as usize);
            }
            if let Some((_, YVal::Map(axes))) = m.iter().find(|(k, _)| k == "per") {
                for (axis, vals) in axes {
                    if let YVal::Map(vals) = vals {
                        for (value, n) in vals {
                            let n = number(&yval(n, dialect, cx, "strategy.max-parallel.per")?).map_or(1, |n| n.max(1.0) as usize);
                            per.push((axis.clone(), value.clone(), n));
                        }
                    }
                }
            }
        }
        Some(v) => max = number(&yval(v, dialect, cx, "strategy.max-parallel")?).map_or(0, |n| n.max(1.0) as usize),
        None => {}
    }
    Ok((ff, max, per))
}

fn group(c: &crate::wf::ir::Concurrency, dialect: Dialect, cx: &Cx, what: &str) -> Result<GroupSpec, String> {
    let group = template(&c.group, dialect, cx, &format!("{what}.group"))?;
    let cancel_in_progress = match &c.cancel_in_progress {
        Some(v) => truthy(&yval(v, dialect, cx, &format!("{what}.cancel-in-progress"))?),
        None => false,
    };
    let queue_max = c.queue.as_deref() == Some("max");
    if queue_max && cancel_in_progress {
        return Err(format!("{what}: `queue: max` can't be combined with `cancel-in-progress: true`"));
    }
    Ok(GroupSpec { group, cancel_in_progress, queue_max, limit: c.limit })
}

/// A `shard`'s or `chunk`'s items: `items:` (an expression giving a list) or `files:` (a filter
/// pattern over the repository).
fn work_items(spec: &YVal, dialect: Dialect, cx: &Cx, src: &dyn PlanSource) -> Result<Vec<String>, String> {
    if let Some(v) = spec.get("items") {
        let val = yval(v, dialect, cx, "items")?;
        return val.items().map(|l| l.iter().map(Value::coerce_string).collect()).ok_or_else(|| "`items` must give a list".into());
    }
    if let Some(f) = spec.get("files").and_then(YVal::as_str) {
        return Ok(src.files(f));
    }
    Err("a shard or chunk needs `items` or `files`".into())
}

#[allow(clippy::type_complexity)]
fn shard_legs(legs: Vec<(Combo, Vec<String>, Vec<String>, bool)>, split: &[Vec<String>], limit: usize) -> Result<Vec<(Combo, Vec<String>, Vec<String>, bool)>, String> {
    let total = split.len();
    if legs.len().saturating_mul(total) > limit {
        return Err(format!("the job would have {} legs; at most {limit} are allowed", legs.len().saturating_mul(total)));
    }
    let mut out = Vec::new();
    for (c, _, req, emu) in legs {
        for (i, items) in split.iter().enumerate() {
            let mut c2 = c.clone();
            c2.push(("shard".into(), Value::object([("index".to_owned(), Value::Number(i as f64)), ("total".to_owned(), Value::Number(total as f64))])));
            out.push((c2, items.clone(), req.clone(), emu));
        }
    }
    Ok(out)
}

/// The needs outputs and results a job's expressions read (W04-R23).
fn reads(job: &Job) -> Vec<String> {
    let mut srcs: Vec<String> = Vec::new();
    srcs.extend(job.cond.iter().map(|t| t.src.clone()));
    srcs.extend(job.env.iter().map(|(_, t)| t.src.clone()));
    srcs.extend(job.outputs.iter().map(|o| o.value.src.clone()));
    for s in &job.steps {
        srcs.extend(s.run.iter().chain(&s.cond).chain(&s.name).map(|t| t.src.clone()));
        srcs.extend(s.env.iter().map(|(_, t)| t.src.clone()));
        let mut ts = Vec::new();
        for (k, v) in &s.with {
            v.templates(k, &mut ts);
        }
        srcs.extend(ts.into_iter().map(|(_, t)| t.src.clone()));
    }
    for (k, v) in &job.with {
        let mut ts = Vec::new();
        v.templates(k, &mut ts);
        srcs.extend(ts.into_iter().map(|(_, t)| t.src.clone()));
    }
    let mut out: Vec<String> = Vec::new();
    for s in srcs {
        let refs = if s.contains("${{") {
            Template::parse(&s, Dialect::Kiln).map(|t| t.references()).unwrap_or_default()
        } else {
            expr::Expr::parse(&s, Dialect::Kiln).map(|e| e.references()).unwrap_or_default()
        };
        for r in refs {
            if r.root().eq_ignore_ascii_case("needs") {
                let path: Vec<String> = r
                    .0
                    .iter()
                    .map(|p| match p {
                        expr::PathSeg::Name(n) => n.clone(),
                        expr::PathSeg::Index(i) => i.to_string(),
                        expr::PathSeg::Any => "*".into(),
                    })
                    .collect();
                let p = path.join(".");
                if !out.contains(&p) {
                    out.push(p);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
