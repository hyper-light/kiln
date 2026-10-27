//! Pipeline definitions: the `.kiln/*.yaml` files that live in each repository.
//!
//! Parsing never panics; every malformed document becomes a [`SpecError`] that is shown on the
//! run in the UI rather than aborting the poller.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum SpecError {
    #[error("yaml: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("job `{job}` needs unknown job `{need}`")]
    UnknownNeed { job: String, need: String },
    #[error("dependency cycle through job `{0}`")]
    Cycle(String),
    #[error("job `{0}` has no steps")]
    NoSteps(String),
    #[error("matrix for job `{job}` expands to {count} combinations (limit {limit})")]
    MatrixTooLarge { job: String, count: usize, limit: usize },
    #[error("job `{job}`'s requires `{expr}`: {why}")]
    Requires { job: String, expr: String, why: String },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pipeline {
    pub name: String,
    #[serde(default)]
    pub on: Triggers,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Only one run of the same concurrency group executes at a time: a newer run cancels the older
    /// ones when `cancel-in-progress` is set, and otherwise waits for the one under way, dropping
    /// an older one that waits too (GitHub's single queue).
    #[serde(default)]
    pub concurrency: Option<Concurrency>,
    pub jobs: BTreeMap<String, JobSpec>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Concurrency {
    pub group: String,
    #[serde(default, rename = "cancel-in-progress")]
    pub cancel_in_progress: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Triggers {
    #[serde(default)]
    pub push: Option<RefFilter>,
    #[serde(default)]
    pub pull_request: Option<RefFilter>,
    #[serde(default)]
    pub tag: Option<RefFilter>,
    /// Cron expressions (`min hour dom mon dow`), evaluated in UTC.
    #[serde(default)]
    pub schedule: Vec<String>,
    /// Allow manual runs from the UI / CLI.
    #[serde(default = "yes")]
    pub manual: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefFilter {
    #[serde(default)]
    pub branches: Vec<String>,
    #[serde(default)]
    pub paths: Vec<String>,
}

impl RefFilter {
    /// Empty filters match everything.
    pub fn matches_branch(&self, branch: &str) -> bool {
        self.branches.is_empty() || self.branches.iter().any(|p| glob_match(p, branch))
    }

    pub fn matches_paths<'a>(&self, mut changed: impl Iterator<Item = &'a str>) -> bool {
        self.paths.is_empty() || changed.any(|f| self.paths.iter().any(|p| glob_match(p, f)))
    }
}

pub fn glob_match(pattern: &str, value: &str) -> bool {
    glob::Pattern::new(pattern).map(|p| p.matches(value)).unwrap_or(pattern == value)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct JobSpec {
    #[serde(default)]
    pub name: Option<String>,
    /// Labels an agent must carry to run this job, e.g. `[k8s]` or `[baremetal, bench]`.
    #[serde(default = "default_runs_on")]
    pub runs_on: Vec<String>,
    #[serde(default)]
    pub needs: Vec<String>,
    /// Preset that chooses a container image and cache dirs: rust, go, python, node, erlang.
    #[serde(default)]
    pub toolchain: Option<String>,
    /// Container image for Kubernetes jobs; ignored on bare metal.
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub matrix: BTreeMap<String, Vec<serde_yaml::Value>>,
    #[serde(default = "default_timeout")]
    pub timeout_minutes: u32,
    #[serde(default)]
    pub retries: u32,
    #[serde(default)]
    pub resources: Resources,
    /// Run only when this expression is truthy (`branch == 'main'`, `event == 'push'`).
    #[serde(default, rename = "if")]
    pub condition: Option<String>,
    /// Bench jobs request an exclusive host: no other job shares the agent while it runs.
    #[serde(default)]
    pub exclusive: bool,
    #[serde(default)]
    pub artifacts: Vec<String>,
    #[serde(default)]
    pub cache: Vec<String>,
    /// The CI engine's benchmark gate (docs/ci.md W16-R09): kept so a file both run stays valid
    /// here, where it's ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bench_gate: Option<serde_yaml::Value>,
    /// Constraints on a runner's facts (docs/ci.md W05-R22: `tools has node`, `disk.free >= 50Gi`),
    /// a string or a list: a job goes only to a runner that meets each, as the CI engine routes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires: Option<serde_yaml::Value>,
    pub steps: Vec<StepSpec>,
}

fn default_runs_on() -> Vec<String> {
    vec!["k8s".to_owned()]
}

fn default_timeout() -> u32 {
    60
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    #[serde(default)]
    pub cpu: Option<String>,
    #[serde(default)]
    pub memory: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct StepSpec {
    #[serde(default)]
    pub name: Option<String>,
    pub run: String,
    #[serde(default)]
    pub shell: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub working_directory: Option<String>,
    #[serde(default)]
    pub timeout_minutes: Option<u32>,
    #[serde(default)]
    pub continue_on_error: bool,
    /// Allocate a pseudo-terminal so tools emit colour and progress bars.
    #[serde(default)]
    pub tty: bool,
}

/// A concrete, matrix-expanded unit of work ready to be queued.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PlannedJob {
    /// `test` or `test (profile=release)`.
    pub key: String,
    /// Job id in the YAML.
    pub spec_id: String,
    pub display: String,
    pub needs: Vec<String>,
    pub labels: Vec<String>,
    pub image: Option<String>,
    pub toolchain: Option<String>,
    pub env: BTreeMap<String, String>,
    pub timeout_minutes: u32,
    pub retries: u32,
    pub exclusive: bool,
    pub resources: Resources,
    pub artifacts: Vec<String>,
    pub cache: Vec<String>,
    pub condition: Option<String>,
    /// What a runner's facts must meet for the job to go there ([`JobSpec::requires`]). Written even
    /// when empty: an assignment carries the job in postcard, which reads each field in turn, so one
    /// left out puts every field after it out of step and the runner refuses the job as malformed.
    /// Only a runner from before the CI work goes without it ([`PlannedJob::for_runner`]).
    #[serde(default)]
    pub requires: Vec<String>,
    pub steps: Vec<StepSpec>,
    /// Written without `requires`, for a runner that reads a job without it; never kept.
    #[serde(skip)]
    pub without_requires: bool,
}

impl PlannedJob {
    /// The job as a runner with capabilities `caps` reads it. One from before the CI work (no
    /// [`crate::proto::CAP_CI2`], W22-AC09's mixed fleet) knows no `requires`, and it's sent only
    /// jobs that require nothing, as it reports no facts to meet a requirement.
    pub fn for_runner(mut self, caps: u32) -> Self {
        self.without_requires = caps & crate::proto::CAP_CI2 == 0;
        self
    }

    /// The job as the runner it's assigned to reads it: [`PlannedJob::for_runner`] with what the
    /// runner said it can do, or for a job pod (`ephemeral`, [`crate::k8s`]) as this build writes
    /// it. A pod never says (it sends no `Build`, so its capabilities read none) and runs the
    /// agent image this server is set up with, which reads `requires`: written without it, every
    /// `[k8s]` job of v1's went to its pod out of step, refused as malformed or read with no steps
    /// and passed without running (W05-R22, W28 step 3's replay).
    pub fn for_assignee(self, caps: u32, ephemeral: bool) -> Self {
        self.for_runner(if ephemeral { caps | crate::proto::CAP_CI2 } else { caps })
    }
}

// By hand for `requires`, which a runner from before the CI work reads a job without: the fields
// in their order, as the derived `Deserialize` reads them.
impl Serialize for PlannedJob {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut j = s.serialize_struct("PlannedJob", 16 + usize::from(!self.without_requires))?;
        j.serialize_field("key", &self.key)?;
        j.serialize_field("spec_id", &self.spec_id)?;
        j.serialize_field("display", &self.display)?;
        j.serialize_field("needs", &self.needs)?;
        j.serialize_field("labels", &self.labels)?;
        j.serialize_field("image", &self.image)?;
        j.serialize_field("toolchain", &self.toolchain)?;
        j.serialize_field("env", &self.env)?;
        j.serialize_field("timeout_minutes", &self.timeout_minutes)?;
        j.serialize_field("retries", &self.retries)?;
        j.serialize_field("exclusive", &self.exclusive)?;
        j.serialize_field("resources", &self.resources)?;
        j.serialize_field("artifacts", &self.artifacts)?;
        j.serialize_field("cache", &self.cache)?;
        j.serialize_field("condition", &self.condition)?;
        if self.without_requires {
            j.skip_field("requires")?;
        } else {
            j.serialize_field("requires", &self.requires)?;
        }
        j.serialize_field("steps", &self.steps)?;
        j.end()
    }
}

pub const MATRIX_LIMIT: usize = 256;

/// Values substituted for `${{ ... }}` expressions.
#[derive(Debug, Clone, Default)]
pub struct Context {
    pub vars: HashMap<String, String>,
}

impl Context {
    pub fn set(&mut self, k: impl Into<String>, v: impl Into<String>) {
        self.vars.insert(k.into(), v.into());
    }

    /// Replace every `${{ name }}`; unknown names become the empty string.
    pub fn interpolate(&self, input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        let mut rest = input;
        while let Some(start) = rest.find("${{") {
            out.push_str(rest.get(..start).unwrap_or_default());
            let after = rest.get(start + 3..).unwrap_or_default();
            match after.find("}}") {
                Some(end) => {
                    let key = after.get(..end).unwrap_or_default().trim();
                    if let Some(v) = self.vars.get(key) {
                        out.push_str(v);
                    }
                    rest = after.get(end + 2..).unwrap_or_default();
                }
                None => {
                    out.push_str(rest.get(start..).unwrap_or_default());
                    rest = "";
                }
            }
        }
        out.push_str(rest);
        out
    }

    /// Minimal condition language: `a == 'b'`, `a != 'b'`, `&&`, `||`, `true`, `false`, bare var.
    pub fn eval(&self, expr: &str) -> bool {
        let expr = self.interpolate(expr);
        expr.split("||").any(|disj| disj.split("&&").all(|t| self.eval_term(t.trim())))
    }

    fn eval_term(&self, term: &str) -> bool {
        let unquote = |s: &str| -> String {
            let s = s.trim();
            let v = s.trim_matches(|c| c == '\'' || c == '"');
            if v.len() != s.len() {
                v.to_owned()
            } else {
                self.vars.get(s).cloned().unwrap_or_else(|| s.to_owned())
            }
        };
        if let Some((a, b)) = term.split_once("!=") {
            return unquote(a) != unquote(b);
        }
        if let Some((a, b)) = term.split_once("==") {
            return unquote(a) == unquote(b);
        }
        match term {
            "" | "true" => true,
            "false" => false,
            other => {
                let (neg, name) = match other.strip_prefix('!') {
                    Some(n) => (true, n.trim()),
                    None => (false, other),
                };
                let v = self.vars.get(name).map(String::as_str).unwrap_or("");
                let truthy = !(v.is_empty() || v == "false" || v == "0");
                truthy != neg
            }
        }
    }
}

pub fn parse(src: &str) -> Result<Pipeline, SpecError> {
    let p: Pipeline = serde_yaml::from_str(src)?;
    validate(&p)?;
    Ok(p)
}

/// A job's `requires`, as written: a string is one, a list each of its strings.
pub fn requirements(v: Option<&serde_yaml::Value>) -> Vec<String> {
    match v {
        Some(serde_yaml::Value::String(r)) => vec![r.clone()],
        Some(serde_yaml::Value::Sequence(rs)) => rs.iter().map(yaml_scalar).collect(),
        _ => Vec::new(),
    }
}

fn validate(p: &Pipeline) -> Result<(), SpecError> {
    for (id, job) in &p.jobs {
        if job.steps.is_empty() {
            return Err(SpecError::NoSteps(id.clone()));
        }
        match &job.requires {
            None | Some(serde_yaml::Value::String(_) | serde_yaml::Value::Sequence(_)) => {}
            Some(other) => {
                let expr = yaml_scalar(other);
                return Err(SpecError::Requires { job: id.clone(), expr, why: "give a constraint, or a list of them".into() });
            }
        }
        for r in requirements(job.requires.as_ref()).iter().filter(|r| !r.contains("${{")) {
            kiln_ci::sched::routing::Constraint::parse(r).map_err(|why| SpecError::Requires { job: id.clone(), expr: r.clone(), why })?;
        }
        for need in &job.needs {
            if !p.jobs.contains_key(need) {
                return Err(SpecError::UnknownNeed { job: id.clone(), need: need.clone() });
            }
        }
    }
    // Kahn's algorithm: anything left over sits on a cycle.
    let mut indeg: BTreeMap<&str, usize> =
        p.jobs.iter().map(|(k, j)| (k.as_str(), j.needs.len())).collect();
    let mut ready: Vec<&str> = indeg.iter().filter(|(_, d)| **d == 0).map(|(k, _)| *k).collect();
    let mut seen = 0usize;
    while let Some(n) = ready.pop() {
        seen += 1;
        for (k, j) in &p.jobs {
            if j.needs.iter().any(|x| x == n)
                && let Some(d) = indeg.get_mut(k.as_str()) {
                    *d = d.saturating_sub(1);
                    if *d == 0 {
                        ready.push(k.as_str());
                    }
                }
        }
    }
    if seen != p.jobs.len() {
        let stuck = indeg.iter().find(|(_, d)| **d > 0).map(|(k, _)| (*k).to_owned());
        return Err(SpecError::Cycle(stuck.unwrap_or_default()));
    }
    Ok(())
}

fn yaml_scalar(v: &serde_yaml::Value) -> String {
    match v {
        serde_yaml::Value::String(s) => s.clone(),
        serde_yaml::Value::Bool(b) => b.to_string(),
        serde_yaml::Value::Number(n) => n.to_string(),
        serde_yaml::Value::Null => String::new(),
        other => serde_yaml::to_string(other).unwrap_or_default().trim().to_owned(),
    }
}

/// A kiln-dialect workflow, read by the new front end (docs/ci.md W02), as v1's model: the bridge
/// that proves the new reader sees every v1 file as v1 does (W02-AC04) until W04's planner
/// replaces [`plan`]. Workflows that use what v1 can't express are refused, naming the key.
pub fn from_ir(wf: &kiln_ci::wf::Workflow) -> Result<Pipeline, String> {
    use kiln_ci::wf::ir::{Axis, Matrix, RunsOn, YVal};
    let num = |v: &YVal, what: &str| match v {
        YVal::Num(n) if *n >= 0.0 && n.fract() == 0.0 && *n <= f64::from(u32::MAX) => Ok(*n as u32),
        _ => Err(format!("{what} is not a whole number v1 can read")),
    };
    let map = |m: &[(String, kiln_ci::wf::ir::Tpl)]| m.iter().map(|(k, v)| (k.clone(), v.src.clone())).collect::<BTreeMap<_, _>>();
    let mut on = Triggers { manual: false, ..Triggers::default() };
    for t in &wf.on {
        let filter = |branches: &Option<Vec<String>>| RefFilter { branches: branches.clone().unwrap_or_default(), paths: t.paths.clone().unwrap_or_default() };
        match t.event.as_str() {
            "push" if t.tags.is_some() => on.tag = Some(filter(&t.tags.as_ref().map(|v| v.iter().filter(|g| *g != "*").cloned().collect()))),
            "push" => on.push = Some(filter(&t.branches)),
            "pull_request" => on.pull_request = Some(filter(&t.branches)),
            "schedule" => on.schedule.extend(t.schedules.iter().map(|s| s.cron.clone())),
            "workflow_dispatch" => on.manual = true,
            e => return Err(format!("on.{e} is not a v1 event")),
        }
    }
    let concurrency = wf.concurrency.as_ref().map(|c| Concurrency { group: c.group.src.clone(), cancel_in_progress: c.cancel_in_progress.as_ref().and_then(YVal::as_bool).unwrap_or(false) });
    let mut jobs = BTreeMap::new();
    for j in &wf.jobs {
        let at = |k: &str| format!("jobs.{}.{k}", j.id);
        let runs_on = match &j.runs_on {
            Some(RunsOn::Labels(l)) => l.iter().map(|t| t.src.clone()).collect(),
            Some(RunsOn::Group { .. }) => return Err(format!("{} uses a runner group", at("runs-on"))),
            None => default_runs_on(),
        };
        let mut matrix = BTreeMap::new();
        match j.strategy.as_ref().and_then(|s| s.matrix.as_ref()) {
            Some(Matrix::Spec(m)) if m.include.is_none() && m.exclude.is_none() && m.filter.is_none() && m.platforms.is_none() => {
                for (k, a) in &m.axes {
                    let Axis::Values(vals) = a else { return Err(format!("{} is an expression", at(&format!("matrix.{k}")))) };
                    matrix.insert(k.clone(), vals.iter().map(yaml_value).collect());
                }
            }
            Some(_) => return Err(format!("{} uses more than v1's axes", at("matrix"))),
            None => {}
        }
        let resources = match &j.kiln.resources {
            Some(r) => resources_of(r).ok_or_else(|| format!("{} is not a mapping", at("resources")))?,
            None => Resources::default(),
        };
        let retries = match j.kiln.retry.as_ref().and_then(|r| r.get("max")) {
            Some(n) => num(n, &at("retries"))?,
            None => 0,
        };
        let mut steps = Vec::new();
        for (i, s) in j.steps.iter().enumerate() {
            let sat = at(&format!("steps[{i}]"));
            let run = s.run.as_ref().ok_or_else(|| format!("{sat} has no `run`: v1 steps are commands"))?;
            steps.push(StepSpec {
                name: s.name.as_ref().map(|t| t.src.clone()),
                run: run.src.clone(),
                shell: s.shell.as_ref().map(|t| t.src.clone()),
                env: map(&s.env),
                working_directory: s.working_directory.as_ref().map(|t| t.src.clone()),
                timeout_minutes: s.timeout_minutes.as_ref().map(|n| num(n, &format!("{sat}.timeout-minutes"))).transpose()?,
                continue_on_error: s.continue_on_error.as_ref().and_then(YVal::as_bool).unwrap_or(false),
                tty: s.kiln.tty,
            });
        }
        let spec = JobSpec {
            name: j.name.as_ref().map(|t| t.src.clone()),
            runs_on,
            needs: j.needs.iter().map(|n| n.job().to_owned()).collect(),
            toolchain: j.kiln.toolchain.clone(),
            image: j.kiln.image.as_ref().map(|t| t.src.clone()),
            env: map(&j.env),
            matrix,
            timeout_minutes: j.timeout_minutes.as_ref().map(|n| num(n, &at("timeout-minutes"))).transpose()?.unwrap_or_else(default_timeout),
            retries,
            resources,
            condition: j.cond.as_ref().map(|t| t.src.clone()),
            exclusive: j.kiln.exclusive,
            artifacts: j.kiln.artifacts.clone(),
            cache: j.kiln.cache.iter().map(|c| c.as_str().map(str::to_owned).ok_or_else(|| format!("{} holds more than directories", at("cache")))).collect::<Result<_, _>>()?,
            // The engine's; v1 doesn't gate on benchmarks.
            bench_gate: None,
            requires: (!j.kiln.requires.is_empty())
                .then(|| serde_yaml::Value::Sequence(j.kiln.requires.iter().map(|t| serde_yaml::Value::String(t.src.clone())).collect())),
            steps,
        };
        jobs.insert(j.id.clone(), spec);
    }
    let p = Pipeline { name: wf.name.clone().unwrap_or_default(), on, env: map(&wf.env), concurrency, jobs };
    validate(&p).map_err(|e| e.to_string())?;
    Ok(p)
}

/// A kiln-dialect job's `resources:` as v1 reads them: its `cpu` and `memory`, as YAML wrote them
/// (`2`, `"500m"`, `16Gi`). `None` when it isn't a mapping. A `k8s` pool's pod for the job reads
/// them so too (docs/ci.md W02-R06, W13-R07).
pub fn resources_of(r: &kiln_ci::wf::ir::YVal) -> Option<Resources> {
    let kiln_ci::wf::ir::YVal::Map(m) = r else { return None };
    let get = |k: &str| m.iter().find(|(x, _)| x == k).map(|(_, v)| yaml_scalar(&yaml_value(v)));
    Some(Resources { cpu: get("cpu"), memory: get("memory") })
}

/// A value of the new model as v1 read it: whole numbers are integers, as YAML wrote them.
fn yaml_value(v: &kiln_ci::wf::ir::YVal) -> serde_yaml::Value {
    use kiln_ci::wf::ir::YVal;
    match v {
        YVal::Null => serde_yaml::Value::Null,
        YVal::Bool(b) => serde_yaml::Value::Bool(*b),
        YVal::Num(n) if n.fract() == 0.0 && n.abs() < 9.0e15 => serde_yaml::Value::Number((*n as i64).into()),
        YVal::Num(n) => serde_yaml::Value::Number((*n).into()),
        YVal::Str(t) => serde_yaml::Value::String(t.src.clone()),
        YVal::Seq(s) => serde_yaml::Value::Sequence(s.iter().map(yaml_value).collect()),
        YVal::Map(m) => serde_yaml::Value::Mapping(m.iter().map(|(k, v)| (serde_yaml::Value::String(k.clone()), yaml_value(v))).collect()),
    }
}

/// Expand matrices and interpolate expressions. `needs` are rewritten to matrix-expanded keys so a
/// job depending on a matrix job waits for every leg.
pub fn plan(p: &Pipeline, base: &Context) -> Result<Vec<PlannedJob>, SpecError> {
    let mut legs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut out = Vec::new();
    for (id, job) in &p.jobs {
        let combos = expand_matrix(id, &job.matrix)?;
        let mut keys = Vec::with_capacity(combos.len());
        for combo in combos {
            let mut ctx = base.clone();
            let mut suffix = Vec::new();
            for (k, v) in &combo {
                ctx.set(format!("matrix.{k}"), v.clone());
                suffix.push(format!("{k}={v}"));
            }
            let key =
                if suffix.is_empty() { id.clone() } else { format!("{id} ({})", suffix.join(", ")) };
            let mut env: BTreeMap<String, String> =
                p.env.iter().map(|(k, v)| (k.clone(), ctx.interpolate(v))).collect();
            for (k, v) in &job.env {
                env.insert(k.clone(), ctx.interpolate(v));
            }
            let steps = job
                .steps
                .iter()
                .map(|s| StepSpec {
                    name: s.name.as_ref().map(|n| ctx.interpolate(n)),
                    run: ctx.interpolate(&s.run),
                    env: s.env.iter().map(|(k, v)| (k.clone(), ctx.interpolate(v))).collect(),
                    working_directory: s.working_directory.as_ref().map(|w| ctx.interpolate(w)),
                    ..s.clone()
                })
                .collect();
            let display = match &job.name {
                Some(n) => {
                    let n = ctx.interpolate(n);
                    if suffix.is_empty() { n } else { format!("{n} ({})", suffix.join(", ")) }
                }
                None => key.clone(),
            };
            keys.push(key.clone());
            out.push(PlannedJob {
                key,
                spec_id: id.clone(),
                display,
                needs: job.needs.clone(),
                labels: job.runs_on.iter().map(|l| ctx.interpolate(l)).collect(),
                image: job.image.as_ref().map(|i| ctx.interpolate(i)),
                toolchain: job.toolchain.clone(),
                env,
                timeout_minutes: job.timeout_minutes.max(1),
                retries: job.retries.min(10),
                exclusive: job.exclusive,
                resources: job.resources.clone(),
                artifacts: job.artifacts.clone(),
                cache: job.cache.clone(),
                condition: job.condition.as_ref().map(|c| ctx.interpolate(c)),
                requires: requirements(job.requires.as_ref()).iter().map(|r| ctx.interpolate(r)).collect(),
                steps,
                without_requires: false,
            });
        }
        legs.insert(id.clone(), keys);
    }
    for j in &mut out {
        j.needs = j
            .needs
            .iter()
            .flat_map(|n| legs.get(n).cloned().unwrap_or_default())
            .collect();
    }
    Ok(out)
}

fn expand_matrix(
    job: &str,
    m: &BTreeMap<String, Vec<serde_yaml::Value>>,
) -> Result<Vec<Vec<(String, String)>>, SpecError> {
    let count = m.values().map(|v| v.len().max(1)).try_fold(1usize, |a, n| a.checked_mul(n));
    let count = count.unwrap_or(usize::MAX);
    if count > MATRIX_LIMIT {
        return Err(SpecError::MatrixTooLarge { job: job.to_owned(), count, limit: MATRIX_LIMIT });
    }
    let mut combos: Vec<Vec<(String, String)>> = vec![Vec::new()];
    for (k, values) in m {
        if values.is_empty() {
            continue;
        }
        let mut next = Vec::with_capacity(combos.len() * values.len());
        for c in &combos {
            for v in values {
                let mut c2 = c.clone();
                c2.push((k.clone(), yaml_scalar(v)));
                next.push(c2);
            }
        }
        combos = next;
    }
    Ok(combos)
}

/// Default container images per toolchain preset (all public, free).
pub fn toolchain_image(toolchain: Option<&str>) -> &'static str {
    match toolchain {
        Some("rust") => "docker.io/library/rust:1-bookworm",
        Some("go") => "docker.io/library/golang:1-bookworm",
        Some("python") => "ghcr.io/astral-sh/uv:python3.14-bookworm",
        Some("node" | "typescript" | "ts") => "docker.io/library/node:24-bookworm",
        Some("bun") => "docker.io/oven/bun:1",
        Some("erlang") => "docker.io/library/erlang:28",
        _ => "docker.io/library/debian:bookworm",
    }
}

/// Cache directories (relative to `$HOME` unless absolute) persisted between runs per toolchain.
pub fn toolchain_cache(toolchain: Option<&str>) -> &'static [&'static str] {
    match toolchain {
        Some("rust") => &[".cargo/registry", ".cargo/git"],
        Some("go") => &["go/pkg/mod", ".cache/go-build"],
        Some("python") => &[".cache/uv"],
        Some("node" | "typescript" | "ts" | "bun") => &[".npm", ".bun/install/cache"],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
name: ci
on:
  push: { branches: [main] }
jobs:
  build:
    toolchain: rust
    matrix: { profile: [dev, release], target: [x86_64] }
    steps:
      - run: "cargo build --profile ${{ matrix.profile }} # ${{ git.sha }}"
  bench:
    needs: [build]
    runs-on: [baremetal, bench]
    exclusive: true
    if: branch == 'main'
    steps:
      - run: cargo bench
"#;

    /// kiln's own `.kiln/*.yaml` read both ways: as v1 pipelines and in the CI engine's kiln
    /// dialect, so neither grows a key the other refuses.
    #[test]
    fn kilns_own_workflows_read_both_ways() -> anyhow::Result<()> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.kiln");
        let mut read = 0;
        for e in std::fs::read_dir(&dir)?.filter_map(Result::ok) {
            let path = e.path();
            if path.extension().is_none_or(|x| x != "yaml") {
                continue;
            }
            let text = std::fs::read_to_string(&path)?;
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            // Not every file there is a pipeline (environments.yaml, routing.yaml, …).
            if !text.lines().any(|l| l.starts_with("jobs:")) {
                continue;
            }
            parse(&text).map_err(|e| anyhow::anyhow!("{name} as v1: {e}"))?;
            let (wf, diags) = kiln_ci::wf::parse_workflow(&format!(".kiln/{name}"), &text, kiln_ci::expr::Dialect::Kiln);
            let errors: Vec<String> = diags.iter().filter(|d| d.is_error()).map(|d| format!("{d:?}")).collect();
            anyhow::ensure!(wf.is_some() && errors.is_empty(), "{name} in the kiln dialect: {errors:?}");
            read += 1;
        }
        anyhow::ensure!(read >= 3, "only {read} of kiln's own workflows read");
        Ok(())
    }

    #[test]
    fn plans_matrix_and_needs() -> Result<(), SpecError> {
        let p = parse(SAMPLE)?;
        let mut ctx = Context::default();
        ctx.set("git.sha", "abc");
        ctx.set("branch", "main");
        let jobs = plan(&p, &ctx)?;
        assert_eq!(jobs.len(), 3);
        let bench = jobs.iter().find(|j| j.spec_id == "bench");
        assert_eq!(bench.map(|b| b.needs.len()), Some(2));
        let built = jobs.iter().any(|j| j.steps.iter().any(|s| s.run.contains("release # abc")));
        assert!(built);
        assert!(ctx.eval(bench.and_then(|b| b.condition.as_deref()).unwrap_or("false")));
        Ok(())
    }

    /// A job's `requires` reach its plan, for dispatch to route by: a string as one, a list as
    /// each, filled in as its labels are. One that isn't a constraint is refused with the pipeline.
    #[test]
    fn a_job_s_requires_reach_its_plan() -> Result<(), SpecError> {
        let p = parse("name: r\njobs:\n  mac:\n    runs-on: [macos]\n    matrix: { tool: [node] }\n    requires: [\"os == macos\", \"tools has ${{ matrix.tool }}\"]\n    steps: [{run: x}]\n  one:\n    requires: tools has cargo\n    steps: [{run: x}]\n")?;
        let jobs = plan(&p, &Context::default())?;
        let of = |id: &str| jobs.iter().find(|j| j.spec_id == id).map(|j| j.requires.clone()).unwrap_or_default();
        assert_eq!(of("mac"), vec!["os == macos".to_owned(), "tools has node".to_owned()]);
        assert_eq!(of("one"), vec!["tools has cargo".to_owned()]);
        let bad = parse("name: r\njobs:\n  a:\n    requires: [\"tools has\"]\n    steps: [{run: x}]\n");
        assert!(matches!(bad, Err(SpecError::Requires { .. })), "{bad:?}");
        Ok(())
    }

    #[test]
    fn rejects_cycles() {
        let src = "name: x\njobs:\n  a: {needs: [b], steps: [{run: x}]}\n  b: {needs: [a], steps: [{run: y}]}\n";
        assert!(matches!(parse(src), Err(SpecError::Cycle(_))));
    }

    #[test]
    fn conditions() {
        let mut c = Context::default();
        c.set("event", "push");
        assert!(c.eval("event == 'push' && true"));
        assert!(!c.eval("event != 'push'"));
        assert!(c.eval("false || event == \"push\""));
        assert!(!c.eval("!event"));
    }
}
