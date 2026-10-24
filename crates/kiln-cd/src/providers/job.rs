//! `job` (docs/cd-targets.md §3): a workflow of the app's repository run as each release's sync,
//! W15's way out for what no provider covers.
//!
//! ```yaml
//! migrate:
//!   job:
//!     workflow: .kiln/deploy-db.yaml   # a workflow of the source's repository
//!     job: migrate                     # one of its jobs, and those it needs (all, by default)
//!     inputs: { version: "${{ release.version }}" }
//! ```
//!
//! - **Run** (`migrate`): `spec` is what a release runs: the workflow, the job, the inputs, and
//!   the release's commit, which the run is of. The release follows that run; a rollback runs the
//!   earlier release's commit.
//! - **Health** is the newest run's.

use serde_json::{Map, Value, json};

use super::runner;
use super::{RenderInput, object, text};
use crate::health::Status;

pub const GROUP: &str = "job.kiln.dev";

/// The files a render reads: none, the run reads its workflow.
pub fn files(_spec: &Value) -> Vec<String> {
    Vec::new()
}

/// An app's objects.
pub fn render(input: &RenderInput<'_>) -> Result<Vec<Value>, String> {
    let workflow = text(input.spec, "workflow").ok_or("job needs workflow: a workflow of the source's repository")?;
    let job = text(input.spec, "job");
    let stem = workflow.rsplit('/').next().unwrap_or(workflow).split('.').next().unwrap_or(workflow);
    let name: String = job.unwrap_or(stem).chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c.to_ascii_lowercase() } else { '-' }).collect();
    let mut spec = Map::new();
    spec.insert("workflow".into(), json!(workflow));
    if let Some(j) = job {
        spec.insert("job".into(), json!(j));
    }
    spec.insert("inputs".into(), Value::Object(runner::text_map(input.spec, "inputs")?));
    spec.insert("revision".into(), json!(input.revision));
    Ok(vec![object(GROUP, "Run", name.trim_matches('-'), None, Value::Object(spec))])
}

pub fn health(o: &Value) -> Option<Status> {
    runner::health(o)
}
