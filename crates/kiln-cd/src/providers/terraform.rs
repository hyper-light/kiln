//! `terraform` (and OpenTofu) (docs/cd-targets.md §3): a workspace of the app's configuration,
//! each release applied by a job on one of kiln's runners.
//!
//! ```yaml
//! infra:
//!   terraform:
//!     workspace: prod
//!     dir: infra                       # the configuration's directory in the source (.)
//!     binary: tofu                     # terraform (the default) or tofu
//!     vars: { image: "${{ release.artifacts.image.digest }}" }
//!     backend-config: { bucket: acme-tfstate }
//!     runs-on: [linux, terraform]      # the runners that have the tool (linux)
//! ```
//!
//! - **Workspace** (`prod`): `spec` is what a release applies: the directory, the binary, the
//!   variables, the backend's settings, and the release's commit, which the job checks out. A
//!   job on a runner inits, selects (or makes) the workspace and applies; the release follows that
//!   run. A rollback applies the earlier release's commit the same way.
//! - **Health** is the newest run's: applied, applying, or why it didn't.

use serde_json::{Map, Value, json};

use super::runner::{self, Job, plain, sq};
use super::{RenderInput, object, text};
use crate::health::Status;

pub const GROUP: &str = "terraform.kiln.dev";

/// The files a render reads: none, the job reads the configuration where it's checked out.
pub fn files(_spec: &Value) -> Vec<String> {
    Vec::new()
}

/// A directory of the source, relative and inside it.
pub fn source_dir(dir: &str) -> Result<String, String> {
    let d = dir.trim().trim_start_matches("./").trim_end_matches('/');
    if d.starts_with('/') || d.split('/').any(|p| p == "..") {
        return Err(format!("dir `{dir}`: a directory inside the source"));
    }
    Ok(if d.is_empty() { ".".into() } else { d.to_owned() })
}

/// An app's objects.
pub fn render(input: &RenderInput<'_>) -> Result<Vec<Value>, String> {
    let ws = text(input.spec, "workspace").ok_or("terraform needs workspace: the workspace each release applies")?;
    if !ws.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')) {
        return Err(format!("workspace `{ws}`: letters, digits, - and _"));
    }
    let binary = text(input.spec, "binary").unwrap_or("terraform");
    if !matches!(binary, "terraform" | "tofu") {
        return Err(format!("binary `{binary}`: terraform or tofu"));
    }
    let mut spec = Map::new();
    spec.insert("dir".into(), json!(source_dir(text(input.spec, "dir").unwrap_or("."))?));
    spec.insert("binary".into(), json!(binary));
    spec.insert("vars".into(), Value::Object(runner::text_map(input.spec, "vars")?));
    spec.insert("backendConfig".into(), Value::Object(runner::text_map(input.spec, "backend-config")?));
    spec.insert("runsOn".into(), runner::runs_on(input.spec, "linux")?);
    spec.insert("revision".into(), json!(input.revision));
    Ok(vec![object(GROUP, "Workspace", ws, None, Value::Object(spec))])
}

/// The workflow that applies a Workspace.
pub fn workflow(o: &Value) -> Result<String, String> {
    let s = |k: &str| o.pointer(&format!("/spec/{k}")).and_then(Value::as_str).unwrap_or_default().to_owned();
    let ws = crate::obj::name(o).to_owned();
    let bin = s("binary");
    let pairs = |k: &str, flag: &str| -> Result<String, String> {
        let mut out = String::new();
        for (name, v) in o.pointer(&format!("/spec/{k}")).and_then(Value::as_object).into_iter().flatten() {
            let v = v.as_str().unwrap_or_default();
            plain(&format!("{k}.{name}"), v)?;
            out.push_str(&format!(" {flag} {}", sq(&format!("{name}={v}"))));
        }
        Ok(out)
    };
    let script = format!(
        "set -eu\ncd {dir}\n{bin} init -input=false -no-color{backend}\n{bin} workspace select -or-create=true {ws}\n{bin} apply -input=false -no-color -auto-approve{vars}\n",
        dir = sq(&s("dir")),
        backend = pairs("backendConfig", "-backend-config")?,
        ws = sq(&ws),
        vars = pairs("vars", "-var")?,
    );
    let job = Job { timeout_minutes: 120, ..Job::script("apply".into(), format!("{bin} apply {ws}"), o.pointer("/spec/runsOn").cloned().unwrap_or_else(|| json!("linux")), script) };
    Ok(runner::workflow(&format!("kiln: {bin} {ws}"), &[job]))
}

pub fn health(o: &Value) -> Option<Status> {
    runner::health(o)
}
