//! Workflow files of both dialects, overlays and action metadata, read into one model and
//! validated (docs/ci.md W02).
//!
//! - [`yaml`]: YAML with positions.
//! - [`diag`]: diagnostics and their codes.
//! - [`keys`]: every key either dialect allows: the one source for checks, schemas and the editor.
//! - [`ir`]: the model.
//! - [`front`]: YAML to the model, per dialect.
//! - [`validate`]: whole-workflow checks (needs, contexts per key, lints).
//! - [`overlay`]: kiln keys for GitHub workflows.
//! - [`action`]: action metadata (`action.yml`).
//! - [`schema`]: JSON Schemas.

pub mod action;
pub mod diag;
pub mod front;
pub mod ir;
pub mod keys;
pub mod overlay;
pub mod schema;
pub mod validate;
pub mod yaml;

use sha2::{Digest as _, Sha256};

pub use diag::{Diag, Severity};
pub use ir::{Action, Job, Step, Workflow};
pub use overlay::Overlay;

use crate::expr::Dialect;

/// What a path in a repository is (W02-R01).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileKind {
    /// `.github/workflows/*.yml` / `*.yaml`.
    GhaWorkflow,
    /// `.kiln/*.yml` / `*.yaml`, other than the policy files.
    KilnWorkflow,
    /// `.kiln/overlays/<stem>.yaml`, for `.github/workflows/<stem>.y(a)ml`.
    Overlay { stem: String },
    /// `.github/actions/**/action.y(a)ml`, `.kiln/actions/**/action.y(a)ml`.
    Action,
    /// trunk's policy files (`.kiln/review.yaml`, …): read by other workstreams, never run.
    Policy,
}

/// The policy files under `.kiln/` that are not workflows (W02-R01).
pub const POLICY_FILES: &[&str] = &["review", "owners", "environments", "agents", "quarantine", "routing", "notify", "deploy", "config", "secrets"];

fn yaml_stem(name: &str) -> Option<&str> {
    name.strip_suffix(".yaml").or_else(|| name.strip_suffix(".yml"))
}

pub fn file_kind(path: &str) -> Option<FileKind> {
    let p = path.trim_start_matches("./");
    if let Some(rest) = p.strip_prefix(".github/workflows/") {
        return (!rest.contains('/') && yaml_stem(rest).is_some()).then_some(FileKind::GhaWorkflow);
    }
    if let Some(rest) = p.strip_prefix(".kiln/overlays/") {
        return yaml_stem(rest).filter(|_| !rest.contains('/')).map(|s| FileKind::Overlay { stem: s.to_owned() });
    }
    let name = p.rsplit('/').next().unwrap_or(p);
    if (p.starts_with(".github/actions/") || p.starts_with(".kiln/actions/")) && (name == "action.yml" || name == "action.yaml") {
        return Some(FileKind::Action);
    }
    if let Some(rest) = p.strip_prefix(".kiln/")
        && !rest.contains('/')
        && let Some(stem) = yaml_stem(rest)
    {
        return Some(if POLICY_FILES.contains(&stem) { FileKind::Policy } else { FileKind::KilnWorkflow });
    }
    None
}

/// The repository setting `ci.github_workflows` (W02-R02).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GithubWorkflows {
    /// Run them when the commit has no kiln-dialect workflow.
    #[default]
    Auto,
    On,
    Off,
}

/// Whether a commit's `.github/workflows` run, and why (W02-R02).
pub fn github_workflows_run(setting: GithubWorkflows, commit_has_kiln_workflow: bool) -> (bool, &'static str) {
    match (setting, commit_has_kiln_workflow) {
        (GithubWorkflows::Off, _) => (false, "GitHub workflows are turned off for this repository"),
        (GithubWorkflows::On, _) => (true, "GitHub workflows run alongside .kiln/ workflows"),
        (GithubWorkflows::Auto, true) => (false, "GitHub workflows not run: this commit has .kiln/ workflows"),
        (GithubWorkflows::Auto, false) => (true, "GitHub workflows run: this commit has no .kiln/ workflows"),
    }
}

/// Reads and checks a workflow. The model is `None` only when the file isn't YAML or isn't a
/// mapping; otherwise it holds whatever could be read, next to every diagnostic.
pub fn parse_workflow(path: &str, src: &str, dialect: Dialect) -> (Option<Workflow>, Vec<Diag>) {
    let mut diags = Vec::new();
    let Some(root) = yaml::parse(src, &mut diags) else {
        if diags.is_empty() {
            diags.push(Diag::error("KCI1014", "the file is empty", diag::Range::default()));
        }
        return (None, diags);
    };
    if root.as_map().is_none() {
        diags.push(Diag::error("KCI1011", format!("a workflow is a mapping, not {}", root.type_name()), root.range));
        return (None, diags);
    }
    let wf = {
        let mut fe = front::Fe { dialect, overlay: false, diags: &mut diags };
        fe.workflow(&root, path)
    };
    validate::validate(&wf, dialect, &mut diags);
    sort(&mut diags);
    (Some(wf), diags)
}

/// Reads an overlay.
pub fn parse_overlay(src: &str) -> (Option<Overlay>, Vec<Diag>) {
    let mut diags = Vec::new();
    let Some(root) = yaml::parse(src, &mut diags) else { return (None, diags) };
    let ov = overlay::parse(&root, &mut diags);
    sort(&mut diags);
    (Some(ov), diags)
}

/// A GHA workflow with its overlay applied, re-validated in the kiln dialect's view of kiln keys.
pub fn apply_overlay(wf: &mut Workflow, ov: &Overlay) -> Vec<Diag> {
    let mut d = overlay::apply(wf, ov);
    sort(&mut d);
    d
}

/// Reads action metadata.
pub fn parse_action(src: &str) -> (Option<Action>, Vec<Diag>) {
    let mut diags = Vec::new();
    let Some(root) = yaml::parse(src, &mut diags) else { return (None, diags) };
    let a = action::parse(&root, &mut diags);
    sort(&mut diags);
    (Some(a), diags)
}

fn sort(d: &mut [Diag]) {
    d.sort_by_key(|x| (x.range.start.line, x.range.start.col, x.code));
}

/// The SHA-256 of the model's canonical JSON (layout and positions excluded): what reuse keys and
/// caches compare (W02-R04).
pub fn def_hash(wf: &Workflow) -> String {
    let json = serde_json::to_vec(wf).unwrap_or_default();
    let d = Sha256::digest(&json);
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// What `kiln validate` and `POST /api/ci/validate` show beside the diagnostics (W02-R12).
pub fn outline(wf: &Workflow) -> serde_json::Value {
    let jobs: Vec<serde_json::Value> = wf
        .jobs
        .iter()
        .map(|j| {
            let legs = match j.strategy.as_ref().and_then(|s| s.matrix.as_ref()) {
                Some(ir::Matrix::Spec(m)) => {
                    let product: usize = m.axes.iter().map(|(_, a)| if let ir::Axis::Values(v) = a { v.len().max(1) } else { 1 }).product();
                    let inc = m.include.as_ref().and_then(ir::YVal::as_seq).map_or(0, <[ir::YVal]>::len);
                    serde_json::json!(if m.axes.is_empty() { inc.max(1) } else { product })
                }
                Some(ir::Matrix::Expr(_)) => serde_json::Value::String("dynamic".into()),
                None => serde_json::json!(1),
            };
            let runs_on: Vec<String> = match &j.runs_on {
                Some(ir::RunsOn::Labels(l)) => l.iter().map(|t| t.src.clone()).collect(),
                Some(ir::RunsOn::Group { group, labels }) => group.iter().chain(labels).map(|t| t.src.clone()).collect(),
                None => Vec::new(),
            };
            serde_json::json!({
                "id": j.id,
                "name": j.name.as_ref().map(|n| n.src.clone()),
                "needs": j.needs.iter().map(|n| n.job().to_owned()).collect::<Vec<_>>(),
                "runs_on": runs_on,
                "uses": j.uses,
                "legs_estimate": legs,
                "steps": j.steps.len(),
            })
        })
        .collect();
    serde_json::json!({
        "name": wf.name,
        "dialect": wf.dialect,
        "events": wf.on.iter().map(|t| t.event.clone()).collect::<Vec<_>>(),
        "jobs": jobs,
    })
}

pub use schema::{SchemaKind, schema};

#[cfg(test)]
mod tests;
