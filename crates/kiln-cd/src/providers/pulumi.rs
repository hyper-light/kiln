//! `pulumi` (docs/cd-targets.md §3): a stack of the app's Pulumi project, each release brought up
//! by a job on one of kiln's runners.
//!
//! ```yaml
//! infra:
//!   pulumi:
//!     stack: acme/shop/prod
//!     dir: infra                       # the project's directory in the source (.)
//!     config: { image: "${{ release.artifacts.image.digest }}" }
//!     runs-on: [linux, pulumi]         # the runners that have the tool (linux)
//! ```
//!
//! - **Stack** (`acme/shop/prod`): `spec` is what a release brings up: the directory, the config
//!   it sets, and the release's commit, which the job checks out. A job selects (or makes) the
//!   stack, sets the config, and runs `pulumi up`; the release follows that run. A rollback brings
//!   the earlier release's commit up the same way.
//! - **Health** is the newest run's.

use serde_json::{Map, Value, json};

use super::runner::{self, Job, sq};
use super::{RenderInput, object, text};
use crate::health::Status;

pub const GROUP: &str = "pulumi.kiln.dev";

/// The files a render reads: none, the job reads the project where it's checked out.
pub fn files(_spec: &Value) -> Vec<String> {
    Vec::new()
}

/// A Stack object's name for a stack (`org/project/stack`): `/` as `.`.
pub fn object_name(stack: &str) -> String {
    stack.replace('/', ".")
}

/// An app's objects.
pub fn render(input: &RenderInput<'_>) -> Result<Vec<Value>, String> {
    let stack = text(input.spec, "stack").ok_or("pulumi needs stack: the stack each release brings up")?;
    if !stack.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/')) {
        return Err(format!("stack `{stack}`: letters, digits, -, _, . and /"));
    }
    let mut spec = Map::new();
    spec.insert("stack".into(), json!(stack));
    spec.insert("dir".into(), json!(super::terraform::source_dir(text(input.spec, "dir").unwrap_or("."))?));
    spec.insert("config".into(), Value::Object(runner::text_map(input.spec, "config")?));
    spec.insert("runsOn".into(), runner::runs_on(input.spec, "linux")?);
    spec.insert("revision".into(), json!(input.revision));
    Ok(vec![object(GROUP, "Stack", &object_name(stack), None, Value::Object(spec))])
}

/// The workflow that brings a Stack up.
pub fn workflow(o: &Value) -> Result<String, String> {
    let s = |k: &str| o.pointer(&format!("/spec/{k}")).and_then(Value::as_str).unwrap_or_default().to_owned();
    let stack = s("stack");
    let mut config = String::new();
    for (k, v) in o.pointer("/spec/config").and_then(Value::as_object).into_iter().flatten() {
        let v = v.as_str().unwrap_or_default();
        runner::plain(&format!("config.{k}"), v)?;
        config.push_str(&format!("pulumi config set --stack {} {} {} --non-interactive\n", sq(&stack), sq(k), sq(v)));
    }
    let script = format!("set -eu\ncd {dir}\npulumi stack select {st} --create --non-interactive\n{config}pulumi up --stack {st} --yes --skip-preview --non-interactive\n", dir = sq(&s("dir")), st = sq(&stack));
    let job = Job { timeout_minutes: 120, ..Job::script("up".into(), format!("pulumi up {stack}"), o.pointer("/spec/runsOn").cloned().unwrap_or_else(|| json!("linux")), script) };
    Ok(runner::workflow(&format!("kiln: pulumi {stack}"), &[job]))
}

pub fn health(o: &Value) -> Option<Status> {
    runner::health(o)
}
