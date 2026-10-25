//! The workflow model (docs/ci.md W02-R04): every key of both dialects (Appendix A.1, Appendix
//! E), one shape for both. Strings that may hold `${{ }}` are [`Tpl`]s, kept as written; they are
//! parsed (and their contexts checked) by [`super::validate`], and evaluated by the planner and
//! the runner. Kiln keys whose meaning belongs to later workstreams are kept as [`YVal`] here and
//! typed by the workstream that implements them. Ranges are skipped when serializing, so a
//! workflow's hash (`def_hash`) changes with its meaning, not its layout.

use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::diag::Range;
use crate::expr::Dialect;

/// A string as written, which may hold `${{ }}` expressions.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Tpl {
    pub src: String,
    #[serde(skip)]
    pub range: Range,
}

impl Tpl {
    pub fn new(src: impl Into<String>, range: Range) -> Self {
        Self { src: src.into(), range }
    }

    pub fn has_expr(&self) -> bool {
        self.src.contains("${{")
    }
}

/// Free-form YAML data (inputs, matrix values, kiln keys typed later), key order kept.
#[derive(Debug, Clone, PartialEq)]
pub enum YVal {
    Null,
    Bool(bool),
    Num(f64),
    Str(Tpl),
    Seq(Vec<YVal>),
    Map(Vec<(String, YVal)>),
}

impl YVal {
    pub fn get(&self, key: &str) -> Option<&YVal> {
        match self {
            YVal::Map(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            YVal::Str(t) => Some(&t.src),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            YVal::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_seq(&self) -> Option<&[YVal]> {
        match self {
            YVal::Seq(s) => Some(s),
            _ => None,
        }
    }

    /// As an expression value (strings stay as written; nothing is evaluated).
    pub fn to_value(&self) -> crate::expr::Value {
        use crate::expr::Value;
        match self {
            YVal::Null => Value::Null,
            YVal::Bool(b) => Value::Bool(*b),
            YVal::Num(n) => Value::Number(*n),
            YVal::Str(t) => Value::str(&t.src),
            YVal::Seq(s) => Value::array(s.iter().map(YVal::to_value).collect()),
            YVal::Map(m) => Value::object(m.iter().map(|(k, v)| (k.clone(), v.to_value()))),
        }
    }

    /// Every template inside, with its path below this value.
    pub fn templates<'a>(&'a self, path: &str, out: &mut Vec<(String, &'a Tpl)>) {
        match self {
            YVal::Str(t) => out.push((path.to_owned(), t)),
            YVal::Seq(s) => {
                for (i, v) in s.iter().enumerate() {
                    v.templates(&format!("{path}[{i}]"), out);
                }
            }
            YVal::Map(m) => {
                for (k, v) in m {
                    v.templates(&format!("{path}.{k}"), out);
                }
            }
            YVal::Null | YVal::Bool(_) | YVal::Num(_) => {}
        }
    }
}

impl Serialize for YVal {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            YVal::Null => s.serialize_unit(),
            YVal::Bool(b) => s.serialize_bool(*b),
            YVal::Num(n) => s.serialize_f64(*n),
            YVal::Str(t) => s.serialize_str(&t.src),
            YVal::Seq(items) => {
                let mut q = s.serialize_seq(Some(items.len()))?;
                for i in items {
                    q.serialize_element(i)?;
                }
                q.end()
            }
            YVal::Map(m) => {
                let mut q = s.serialize_map(Some(m.len()))?;
                for (k, v) in m {
                    q.serialize_entry(k, v)?;
                }
                q.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for YVal {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = YVal;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("any YAML value")
            }
            fn visit_unit<E>(self) -> Result<YVal, E> {
                Ok(YVal::Null)
            }
            fn visit_none<E>(self) -> Result<YVal, E> {
                Ok(YVal::Null)
            }
            fn visit_bool<E>(self, b: bool) -> Result<YVal, E> {
                Ok(YVal::Bool(b))
            }
            fn visit_f64<E>(self, n: f64) -> Result<YVal, E> {
                Ok(YVal::Num(n))
            }
            #[allow(clippy::cast_precision_loss)]
            fn visit_i64<E>(self, n: i64) -> Result<YVal, E> {
                Ok(YVal::Num(n as f64))
            }
            #[allow(clippy::cast_precision_loss)]
            fn visit_u64<E>(self, n: u64) -> Result<YVal, E> {
                Ok(YVal::Num(n as f64))
            }
            fn visit_str<E>(self, s: &str) -> Result<YVal, E> {
                Ok(YVal::Str(Tpl::new(s, Range::default())))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<YVal, A::Error> {
                let mut out = Vec::new();
                while let Some(v) = a.next_element()? {
                    out.push(v);
                }
                Ok(YVal::Seq(out))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<YVal, A::Error> {
                let mut out = Vec::new();
                while let Some((k, v)) = a.next_entry::<String, YVal>()? {
                    out.push((k, v));
                }
                Ok(YVal::Map(out))
            }
        }
        d.deserialize_any(V)
    }
}

/// A workflow file, of either dialect.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Workflow {
    pub dialect: Option<Dialect>,
    pub path: String,
    pub name: Option<String>,
    pub run_name: Option<Tpl>,
    pub on: Vec<Trigger>,
    pub permissions: Option<Permissions>,
    pub env: Vec<(String, Tpl)>,
    pub defaults: Defaults,
    pub concurrency: Option<Concurrency>,
    /// `read`, `write`, `write-only` or `none`.
    pub cache_mode: Option<String>,
    pub jobs: Vec<Job>,
    /// kiln: `secrets:` the workflow declares (W09-R06).
    pub secrets: Vec<SecretDecl>,
    /// kiln: `supersede:` (W05-R08).
    pub supersede: Option<YVal>,
    /// kiln: `routing: { infer }` (W05-R19).
    pub routing_infer: bool,
    /// kiln: `run-env: true` (W04-R21).
    pub run_env: bool,
    /// kiln: `kiln: { honour-skip-ci }` (W03-R06).
    pub honour_skip_ci: bool,
    /// kiln: `kiln: { chain-depth }`, how deep a chain of `workflow_run` (and `pipeline`) runs
    /// this workflow takes part in: 3 when unset (GitHub's), at most 10 (W03-R13).
    pub chain_depth: Option<u32>,
    /// kiln: the implicit checkout (W06-R14); GHA workflows never have one.
    pub checkout: bool,
}

impl Workflow {
    pub fn job(&self, id: &str) -> Option<&Job> {
        self.jobs.iter().find(|j| j.id == id)
    }

    pub fn trigger(&self, event: &str) -> Option<&Trigger> {
        self.on.iter().find(|t| t.event == event)
    }
}

/// One event under `on:`, with every filter and setting any event takes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Trigger {
    pub event: String,
    #[serde(skip)]
    pub range: Range,
    pub types: Vec<String>,
    pub branches: Option<Vec<String>>,
    pub branches_ignore: Option<Vec<String>>,
    pub tags: Option<Vec<String>>,
    pub tags_ignore: Option<Vec<String>>,
    pub paths: Option<Vec<String>>,
    pub paths_ignore: Option<Vec<String>>,
    /// `workflow_dispatch`, `workflow_call` and kiln's `agent` inputs.
    pub inputs: Vec<Input>,
    /// `workflow_call` outputs.
    pub outputs: Vec<CallOutput>,
    /// `workflow_call` secrets.
    pub secrets: Vec<SecretDecl>,
    /// `workflow_run` workflows.
    pub workflows: Vec<String>,
    /// `schedule` entries.
    pub schedules: Vec<Schedule>,
    /// Kiln events' other filters (`drafts`, `authors`, `states`, …), as written.
    pub extra: Vec<(String, YVal)>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Input {
    pub name: String,
    pub description: Option<String>,
    pub required: bool,
    pub default: Option<YVal>,
    #[serde(rename = "type")]
    pub ty: Option<String>,
    pub options: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CallOutput {
    pub name: String,
    pub description: Option<String>,
    pub value: Tpl,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SecretDecl {
    pub name: String,
    pub description: Option<String>,
    pub required: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Schedule {
    pub cron: String,
    #[serde(skip)]
    pub range: Range,
    pub timezone: Option<String>,
    pub branch: Option<String>,
    pub inputs: Vec<(String, YVal)>,
    pub jitter: Option<String>,
    pub skip_if_unchanged: bool,
    pub catch_up: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Defaults {
    pub shell: Option<Tpl>,
    pub working_directory: Option<Tpl>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Concurrency {
    pub group: Tpl,
    /// A boolean or an expression.
    pub cancel_in_progress: Option<YVal>,
    /// `single` (GitHub's default) or `max`.
    pub queue: Option<String>,
    /// kiln (W05-R07): at most this many in progress.
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Permissions {
    ReadAll,
    WriteAll,
    Scopes(Vec<(String, String)>),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    #[serde(skip)]
    pub range: Range,
    pub name: Option<Tpl>,
    pub needs: Vec<Need>,
    /// `if:`
    pub cond: Option<Tpl>,
    pub runs_on: Option<RunsOn>,
    pub environment: Option<Environment>,
    pub concurrency: Option<Concurrency>,
    pub outputs: Vec<Output>,
    pub env: Vec<(String, Tpl)>,
    pub defaults: Defaults,
    pub steps: Vec<Step>,
    /// A number or an expression.
    pub timeout_minutes: Option<YVal>,
    pub strategy: Option<Strategy>,
    /// A boolean or an expression.
    pub continue_on_error: Option<YVal>,
    pub container: Option<Container>,
    pub services: Vec<(String, Container)>,
    /// A reusable workflow this job calls.
    pub uses: Option<String>,
    pub with: Vec<(String, YVal)>,
    pub secrets: Option<JobSecrets>,
    pub permissions: Option<Permissions>,
    /// GitHub's custom-image `snapshot:` (kept as written).
    pub snapshot: Option<YVal>,
    pub cache_mode: Option<String>,
    pub kiln: JobKiln,
}

/// kiln's job keys (Appendix E).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JobKiln {
    pub toolchain: Option<String>,
    pub image: Option<Tpl>,
    pub exclusive: bool,
    pub resources: Option<YVal>,
    pub requires: Vec<Tpl>,
    pub prefer: Vec<YVal>,
    pub priority: Option<YVal>,
    pub retry: Option<YVal>,
    pub reuse: Option<YVal>,
    /// `work:` (W24), as written: its `warm` read by `work::slots::warm`. Left out of the model's
    /// JSON when unset, so a workflow without it hashes as it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work: Option<YVal>,
    pub cache: Vec<YVal>,
    pub exports_env: Vec<String>,
    pub env_from: Vec<String>,
    pub integrations: Vec<String>,
    pub registries: Vec<String>,
    pub network: Option<YVal>,
    pub debug: Option<YVal>,
    pub attest: Option<YVal>,
    pub quarantine_enforce: bool,
    pub bench_gate: Option<YVal>,
    pub deploy: Option<YVal>,
    /// `discover`: the usual reports, after the job's steps (W16-R01).
    pub test_results: Option<YVal>,
    pub agents: Option<YVal>,
    /// v1's `artifacts: [globs]`.
    pub artifacts: Vec<String>,
    /// `checkout: false` turns the kiln dialect's implicit checkout off for this job.
    pub checkout: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Need {
    Job(String),
    /// kiln: `{ job, matrix: same }` or `{ job, where }` (W04-R17).
    Legs { job: String, matrix_same: bool, filter: Option<Tpl> },
}

impl Need {
    pub fn job(&self) -> &str {
        match self {
            Need::Job(j) | Need::Legs { job: j, .. } => j,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunsOn {
    Labels(Vec<Tpl>),
    Group { group: Option<Tpl>, labels: Vec<Tpl> },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Environment {
    pub name: Tpl,
    pub url: Option<Tpl>,
    /// `false`: no deployment record (a boolean or an expression).
    pub deployment: Option<YVal>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Output {
    pub name: String,
    pub value: Tpl,
    /// kiln (W04-R22).
    pub secret: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Step {
    #[serde(skip)]
    pub range: Range,
    /// Where the step's `env` mapping is, when it has one (for fixes that add to it).
    #[serde(skip)]
    pub env_range: Option<Range>,
    pub id: Option<String>,
    pub name: Option<Tpl>,
    pub cond: Option<Tpl>,
    pub uses: Option<String>,
    pub run: Option<Tpl>,
    pub shell: Option<Tpl>,
    pub with: Vec<(String, YVal)>,
    pub env: Vec<(String, Tpl)>,
    pub working_directory: Option<Tpl>,
    pub continue_on_error: Option<YVal>,
    pub timeout_minutes: Option<YVal>,
    /// Runs in the background (a boolean or an expression).
    pub background: Option<YVal>,
    /// Waits for these background steps.
    pub wait: Vec<String>,
    /// Waits for every earlier background step.
    pub wait_all: bool,
    /// Stops these background steps.
    pub cancel: Vec<String>,
    /// Steps run as background steps with an implicit wait after them.
    pub parallel: Vec<Step>,
    pub kiln: StepKiln,
}

impl Step {
    /// A `wait`, `wait-all`, `cancel` or `parallel` step: it runs no command or action itself.
    pub fn is_control(&self) -> bool {
        !self.wait.is_empty() || self.wait_all || !self.cancel.is_empty() || !self.parallel.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StepKiln {
    pub retry: Option<YVal>,
    pub with_secrets: Vec<(String, String)>,
    pub test_results: Option<YVal>,
    pub integrations: Vec<String>,
    pub tty: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Strategy {
    pub matrix: Option<Matrix>,
    /// A boolean or an expression (kiln: also an object, W04-R16).
    pub fail_fast: Option<YVal>,
    /// A number or an expression (kiln: also an object).
    pub max_parallel: Option<YVal>,
    /// kiln (W04-R12, R14, R15).
    pub when: Option<Tpl>,
    pub shard: Option<YVal>,
    pub chunk: Option<YVal>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Matrix {
    /// `matrix: ${{ fromJSON(…) }}`
    Expr(Tpl),
    Spec(MatrixSpec),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MatrixSpec {
    pub axes: Vec<(String, Axis)>,
    pub include: Option<YVal>,
    pub exclude: Option<YVal>,
    /// kiln (W04-R11, R13).
    #[serde(rename = "where")]
    pub filter: Option<Tpl>,
    pub platforms: Option<Axis>,
    pub emulate: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    Values(Vec<YVal>),
    Expr(Tpl),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Container {
    pub image: Tpl,
    pub credentials: Option<(Tpl, Tpl)>,
    pub env: Vec<(String, Tpl)>,
    pub ports: Vec<YVal>,
    pub volumes: Vec<Tpl>,
    pub options: Option<Tpl>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobSecrets {
    Inherit,
    Map(Vec<(String, Tpl)>),
}

/// An action's metadata (`action.yml`, W02-R10).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Action {
    pub name: Option<String>,
    pub description: Option<String>,
    pub author: Option<String>,
    pub inputs: Vec<ActionInput>,
    pub outputs: Vec<ActionOutput>,
    pub runs: ActionRuns,
    pub branding: Option<YVal>,
    /// kiln actions (W07-R11, W09-R07).
    pub secrets: Vec<SecretDecl>,
    pub requires: Vec<String>,
    pub permissions: Option<Permissions>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ActionInput {
    pub name: String,
    pub description: Option<String>,
    pub required: bool,
    pub default: Option<Tpl>,
    pub deprecation_message: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ActionOutput {
    pub name: String,
    pub description: Option<String>,
    /// Composite actions only.
    pub value: Option<Tpl>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ActionRuns {
    /// `node12`, `node16`, `node20`, `node24`, `composite`, `docker`.
    pub using: String,
    pub main: Option<String>,
    pub pre: Option<String>,
    pub post: Option<String>,
    pub pre_if: Option<Tpl>,
    pub post_if: Option<Tpl>,
    pub image: Option<String>,
    pub entrypoint: Option<String>,
    pub pre_entrypoint: Option<String>,
    pub post_entrypoint: Option<String>,
    pub args: Vec<Tpl>,
    pub env: Vec<(String, Tpl)>,
    pub steps: Vec<Step>,
}
