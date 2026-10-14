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
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pipeline {
    pub name: String,
    #[serde(default)]
    pub on: Triggers,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Only one run of the same concurrency group executes at a time; newer runs cancel older
    /// queued ones when `cancel-in-progress` is set.
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
    pub steps: Vec<StepSpec>,
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

fn validate(p: &Pipeline) -> Result<(), SpecError> {
    for (id, job) in &p.jobs {
        if job.steps.is_empty() {
            return Err(SpecError::NoSteps(id.clone()));
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
                steps,
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
