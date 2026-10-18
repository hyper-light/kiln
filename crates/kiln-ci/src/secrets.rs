//! Secrets and variables (docs/ci.md W09), the deciding half.
//!
//! - Names and scopes, and which scope wins (W09-R01, W09-R02).
//! - Which secrets a leg's expressions read, so it is given those and no others (W09-R04). A
//!   reference this can't pin down (`secrets[expr]`, `toJSON(secrets)`) means every secret, as
//!   GitHub gives them all.
//! - What a called workflow's leg gets through `secrets: inherit` or its caller's map (W09-R05).
//! - A kiln workflow's declared secrets (W09-R06).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::expr::{self, Budget, Dialect, MapContexts, Template, Value};
use crate::plan::SecretsSpec;
use crate::wf::ir::{Container, Job, JobSecrets, SecretDecl, Step, Tpl, Workflow};

/// Where a secret or variable is defined, from the least specific to the most.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    Server,
    Organization,
    Repository,
    /// One workflow file of one repository: the pipeline scope (W09-R06).
    Workflow,
    Environment,
}

impl ScopeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ScopeKind::Server => "server",
            ScopeKind::Organization => "organization",
            ScopeKind::Repository => "repository",
            ScopeKind::Workflow => "workflow",
            ScopeKind::Environment => "environment",
        }
    }

    pub fn parse(s: &str) -> Option<ScopeKind> {
        Some(match s {
            "server" => ScopeKind::Server,
            "organization" | "org" => ScopeKind::Organization,
            "repository" | "repo" => ScopeKind::Repository,
            "workflow" => ScopeKind::Workflow,
            "environment" | "env" => ScopeKind::Environment,
            _ => return None,
        })
    }
}

/// The largest secret value (W09-R02).
pub const MAX_SECRET: usize = 64 << 10;
/// The largest variable value (W09-R09).
pub const MAX_VARIABLE: usize = 48 << 10;
/// Secrets or variables per scope.
pub const MAX_PER_SCOPE: usize = 1000;

/// A name as stored (upper-case), or why it can't be one (W09-R02, GitHub-exact).
pub fn check_name(name: &str) -> Result<String, String> {
    let n = name.trim();
    let mut chars = n.chars();
    let ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !ok {
        return Err(format!("`{n}` is not a valid name: alphanumeric characters or underscores only, and not starting with a number"));
    }
    let up = n.to_ascii_uppercase();
    if up.starts_with("GITHUB_") {
        return Err("Names must not start with GITHUB_.".into());
    }
    if up.starts_with("KILN_") {
        return Err("Names must not start with KILN_.".into());
    }
    Ok(up)
}

/// The scope a name resolves to, among those that hold it: the most specific (W09-R01).
pub fn winner(held: &[ScopeKind]) -> Option<ScopeKind> {
    held.iter().copied().max()
}

/// The secrets expressions read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Used {
    /// Upper-case names.
    pub names: BTreeSet<String>,
    /// A reference that could be any secret.
    pub all: bool,
}

impl Used {
    /// The references of one template or expression source.
    pub fn add(&mut self, src: &str) {
        if !src.to_ascii_lowercase().contains("secrets") {
            return;
        }
        let refs = if src.contains("${{") { Template::parse(src, Dialect::Kiln).map(|t| t.references()).unwrap_or_default() } else { expr::Expr::parse(src, Dialect::Kiln).map(|e| e.references()).unwrap_or_default() };
        for r in refs.iter().filter(|r| r.root() == "secrets") {
            match r.name(1) {
                Some(n) => {
                    self.names.insert(n.to_ascii_uppercase());
                }
                None => self.all = true,
            }
        }
    }

    fn tpl(&mut self, t: &Tpl) {
        self.add(&t.src);
    }

    fn container(&mut self, c: &Container) {
        self.tpl(&c.image);
        if let Some((u, p)) = &c.credentials {
            self.tpl(u);
            self.tpl(p);
        }
        c.env.iter().for_each(|(_, t)| self.tpl(t));
        c.volumes.iter().for_each(|t| self.tpl(t));
        if let Some(o) = &c.options {
            self.tpl(o);
        }
    }

    fn step(&mut self, s: &Step) {
        for t in s.run.iter().chain(&s.cond).chain(&s.name).chain(&s.working_directory).chain(&s.shell) {
            self.tpl(t);
        }
        s.env.iter().for_each(|(_, t)| self.tpl(t));
        let mut ts = Vec::new();
        for (k, v) in &s.with {
            v.templates(k, &mut ts);
        }
        ts.into_iter().for_each(|(_, t)| self.tpl(t));
        // A kiln action's declared secrets come from the job's (W09-R07).
        for (_, from) in &s.kiln.with_secrets {
            self.names.insert(from.to_ascii_uppercase());
        }
        s.parallel.iter().for_each(|p| self.step(p));
    }
}

/// The secrets a job reads (W09-R04): everywhere a step or the job may use them.
pub fn used(job: &Job) -> Used {
    let mut u = Used::default();
    for t in job.cond.iter() {
        u.tpl(t);
    }
    job.env.iter().for_each(|(_, t)| u.tpl(t));
    job.outputs.iter().for_each(|o| u.tpl(&o.value));
    job.steps.iter().for_each(|s| u.step(s));
    if let Some(c) = &job.container {
        u.container(c);
    }
    job.services.iter().for_each(|(_, c)| u.container(c));
    let mut ts = Vec::new();
    for (k, v) in &job.with {
        v.templates(k, &mut ts);
    }
    ts.into_iter().for_each(|(_, t)| u.tpl(t));
    if let Some(JobSecrets::Map(m)) = &job.secrets {
        m.iter().for_each(|(_, t)| u.tpl(t));
    }
    u
}

/// A template evaluated with only a `secrets` context (a caller's `secrets:` map).
fn render(src: &str, secrets: &BTreeMap<String, String>) -> Option<String> {
    let t = Template::parse(src, Dialect::Gha).ok()?;
    let cx = MapContexts::default().with("secrets", Value::object(secrets.iter().map(|(k, v)| (k.clone(), Value::str(v)))));
    expr::render(&t, &cx, &mut Budget::default()).ok()
}

/// The `secrets` context a leg gets (W09-R04, W09-R05): what its job reads, followed through the
/// calls that brought it (`secrets: inherit` or the caller's map) to the run's secrets `run`
/// (upper-case names). `GITHUB_TOKEN` comes from `run` like any other.
pub fn deliver(spec: &SecretsSpec, used: &Used, run: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    match spec {
        SecretsSpec::Run => {
            if used.all {
                run.clone()
            } else {
                used.names.iter().filter_map(|n| run.get(n).map(|v| (n.clone(), v.clone()))).collect()
            }
        }
        SecretsSpec::None => BTreeMap::new(),
        SecretsSpec::Mapped { map, from } => {
            let mut out = BTreeMap::new();
            for (callee, src) in map {
                let name = callee.to_ascii_uppercase();
                if !(used.all || used.names.contains(&name)) {
                    continue;
                }
                let mut caller = Used::default();
                caller.add(src);
                let given = deliver(from, &caller, run);
                if let Some(v) = render(src, &given) {
                    out.insert(name, v);
                }
            }
            out
        }
    }
}

/// The run-level names a leg's delivery draws on: what to resolve and decrypt for it.
pub fn needed(spec: &SecretsSpec, used: &Used) -> Used {
    match spec {
        SecretsSpec::Run => used.clone(),
        SecretsSpec::None => Used::default(),
        SecretsSpec::Mapped { map, from } => {
            let mut caller = Used::default();
            for (callee, src) in map {
                if used.all || used.names.contains(&callee.to_ascii_uppercase()) {
                    caller.add(src);
                }
            }
            needed(from, &caller)
        }
    }
}

/// A kiln workflow's declared secrets (W09-R06): the required ones missing from `available`.
pub fn missing_required(decls: &[SecretDecl], available: &BTreeSet<String>) -> Vec<String> {
    decls.iter().filter(|d| d.required).map(|d| d.name.to_ascii_uppercase()).filter(|n| !available.contains(n)).collect()
}

/// References to secrets a workflow that declares its secrets doesn't declare (`KCI4041`).
/// Empty when it declares none (then it may use any).
pub fn undeclared(wf: &Workflow) -> Vec<String> {
    if wf.secrets.is_empty() {
        return Vec::new();
    }
    let declared: BTreeSet<String> = wf.secrets.iter().map(|d| d.name.to_ascii_uppercase()).chain(["GITHUB_TOKEN".to_owned()]).collect();
    let mut out: BTreeSet<String> = BTreeSet::new();
    for j in &wf.jobs {
        out.extend(used(j).names.into_iter().filter(|n| !declared.contains(n)));
    }
    out.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wf;

    fn job(src: &str) -> Result<Job, String> {
        let (w, d) = wf::parse_workflow(".github/workflows/t.yml", src, Dialect::Gha);
        if d.iter().any(|x| x.is_error()) {
            return Err(format!("{d:?}"));
        }
        w.and_then(|w| w.jobs.into_iter().next()).ok_or_else(|| "no job".into())
    }

    fn run_secrets() -> BTreeMap<String, String> {
        [("NPM", "n"), ("DEPLOY", "d"), ("OTHER", "o"), ("GITHUB_TOKEN", "t")].iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    #[test]
    fn w09_ac03_the_most_specific_scope_wins() {
        use ScopeKind::*;
        assert_eq!(winner(&[Server, Repository, Organization]), Some(Repository));
        assert_eq!(winner(&[Environment, Server, Workflow]), Some(Environment));
        assert_eq!(winner(&[Workflow, Repository]), Some(Workflow));
        assert_eq!(winner(&[]), None);
        assert_eq!(check_name("npm_token").as_deref(), Ok("NPM_TOKEN"));
        assert!(check_name("github_x").is_err());
        assert!(check_name("KILN_X").is_err());
        assert!(check_name("1abc").is_err());
        assert!(check_name("a-b").is_err());
    }

    #[test]
    fn w09_ac04_a_leg_reads_only_what_it_references() -> Result<(), String> {
        let j = job("on: push\njobs:\n  a:\n    runs-on: x\n    env:\n      T: ${{ secrets.npm }}\n    steps:\n      - run: echo ${{ secrets.DEPLOY }}\n      - uses: ./x\n        with:\n          k: ${{ secrets.npm }}\n")?;
        let u = used(&j);
        assert_eq!(u.names.iter().cloned().collect::<Vec<_>>(), vec!["DEPLOY", "NPM"]);
        assert!(!u.all);
        let got = deliver(&SecretsSpec::Run, &u, &run_secrets());
        assert_eq!(got.keys().cloned().collect::<Vec<_>>(), vec!["DEPLOY", "NPM"]);
        // A computed name or the whole context: every secret.
        let all = used(&job("on: push\njobs:\n  a:\n    runs-on: x\n    steps:\n      - run: echo '${{ toJSON(secrets) }}'\n")?);
        assert!(all.all);
        assert_eq!(deliver(&SecretsSpec::Run, &all, &run_secrets()).len(), 4);
        let computed = used(&job("on: push\njobs:\n  a:\n    runs-on: x\n    steps:\n      - run: echo ${{ secrets[format('{0}', 'NPM')] }}\n")?);
        assert!(computed.all);
        Ok(())
    }

    #[test]
    fn w09_r05_called_workflows_get_inherited_or_mapped_secrets() {
        let mut u = Used::default();
        u.add("${{ secrets.TOKEN }} ${{ secrets.NPM }}");
        // Mapped: the callee's TOKEN is the caller's DEPLOY; NPM isn't passed.
        let mapped = SecretsSpec::Mapped { map: vec![("token".into(), "${{ secrets.DEPLOY }}".into())], from: Box::new(SecretsSpec::Run) };
        let got = deliver(&mapped, &u, &run_secrets());
        assert_eq!(got, BTreeMap::from([("TOKEN".to_owned(), "d".to_owned())]));
        assert_eq!(needed(&mapped, &u).names.iter().cloned().collect::<Vec<_>>(), vec!["DEPLOY"]);
        // Two levels of calls.
        let twice = SecretsSpec::Mapped { map: vec![("TOKEN".into(), "${{ secrets.MID }}".into())], from: Box::new(SecretsSpec::Mapped { map: vec![("MID".into(), "${{ secrets.OTHER }}".into())], from: Box::new(SecretsSpec::Run) }) };
        assert_eq!(deliver(&twice, &u, &run_secrets()).get("TOKEN").map(String::as_str), Some("o"));
        // No secrets passed: none.
        assert!(deliver(&SecretsSpec::None, &u, &run_secrets()).is_empty());
    }

    #[test]
    fn w09_r06_declared_secrets() {
        let decls = vec![SecretDecl { name: "npm_token".into(), description: None, required: true }, SecretDecl { name: "sentry".into(), description: None, required: false }];
        let have: BTreeSet<String> = ["SENTRY".to_owned()].into_iter().collect();
        assert_eq!(missing_required(&decls, &have), vec!["NPM_TOKEN"]);
    }
}
