//! `compose` (docs/cd-targets.md §3): a Compose project on the hosts kiln's runners run on, with
//! Docker, each release brought up a batch of hosts at a time.
//!
//! ```yaml
//! web:
//!   compose:
//!     project: shop
//!     file: compose.yaml               # found there by default; or the file as an object
//!     env: { IMAGE: "ghcr.io/acme/shop@${{ release.artifacts.image.digest }}" }
//!     batch: 1                         # hosts at a time (1)
//! ```
//!
//! - **Project** (`shop`): `spec` is the Compose file and the environment it's read with. On each
//!   host the target's selector picks, a job of the runner there writes them into the project's
//!   directory and runs `docker compose up --wait`, which waits for the services' health checks;
//!   a batch starts once the one before passed. A rollback brings the earlier release's file up
//!   the same way.
//! - **Health** is the newest run's.

use serde_json::{Map, Value, json};

use super::host::Host;
use super::runner::{self, Job, plain, sq};
use super::{RenderInput, object, text};
use crate::health::Status;

pub const GROUP: &str = "compose.kiln.dev";

/// The Compose file's place when the definition names none.
pub const DEFAULT_FILE: &str = "compose.yaml";

/// The files a render reads.
pub fn files(spec: &Value) -> Vec<String> {
    match spec.get("file") {
        Some(Value::String(p)) => vec![p.clone()],
        Some(_) => Vec::new(),
        None => vec![DEFAULT_FILE.to_owned()],
    }
}

/// An app's objects.
pub fn render(input: &RenderInput<'_>) -> Result<Vec<Value>, String> {
    let project = text(input.spec, "project").ok_or("compose needs project: the Compose project")?;
    if !project.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_')) || project.is_empty() {
        return Err(format!("project `{project}`: lowercase letters, digits, - and _, as Compose names projects"));
    }
    let file = match input.spec.get("file") {
        Some(Value::String(path)) => String::from_utf8_lossy(&(input.read)(path).ok_or_else(|| format!("file: no {path} in the source"))?).into_owned(),
        Some(v @ Value::Object(_)) => serde_yaml::to_string(v).map_err(|e| format!("file: {e}"))?,
        Some(_) => return Err("file: the Compose file in the source, or the file as an object".into()),
        None => String::from_utf8_lossy(&(input.read)(DEFAULT_FILE).ok_or("compose needs its file: compose.yaml in the source, or file:")?).into_owned(),
    };
    let parsed: serde_yaml::Value = serde_yaml::from_str(&file).map_err(|e| format!("the Compose file: {e}"))?;
    if parsed.get("services").is_none() {
        return Err("the Compose file has no services".into());
    }
    plain("the Compose file", &file)?;
    let mut spec = Map::new();
    spec.insert("compose".into(), json!(file));
    spec.insert("env".into(), Value::Object(runner::text_map(input.spec, "env")?));
    let batch = input.spec.get("batch").and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))).unwrap_or(1).max(1);
    spec.insert("batch".into(), json!(batch));
    Ok(vec![object(GROUP, "Project", project, None, Value::Object(spec))])
}

/// A host's script: the file and its environment in the project's directory, then
/// `docker compose up --wait`.
fn script(o: &Value, down: bool) -> String {
    let project = crate::obj::name(o);
    let file = o.pointer("/spec/compose").and_then(Value::as_str).unwrap_or_default();
    let env: String = o.pointer("/spec/env").and_then(Value::as_object).into_iter().flatten().map(|(k, v)| format!("{k}={}\n", v.as_str().unwrap_or_default())).collect();
    let dir = format!("\"$HOME/.kiln/compose/\"{}", sq(project));
    if down {
        return format!("set -eu\nD={dir}\nif [ -f \"$D/compose.yaml\" ]; then docker compose -p {p} -f \"$D/compose.yaml\" down --remove-orphans; rm -rf \"$D\"; fi\n", p = sq(project));
    }
    format!(
        "set -eu\nD={dir}\nmkdir -p \"$D\"\ncat > \"$D/compose.yaml\" <<'KILN_COMPOSE_FILE'\n{file}\nKILN_COMPOSE_FILE\ncat > \"$D/.env\" <<'KILN_COMPOSE_ENV'\n{env}KILN_COMPOSE_ENV\ndocker compose -p {p} -f \"$D/compose.yaml\" --env-file \"$D/.env\" up -d --wait --remove-orphans\n",
        file = file.trim_end(),
        p = sq(project),
    )
}

/// The workflow that brings a Project up on `hosts` (or takes it down, `down`), a batch at a time.
pub fn workflow(o: &Value, hosts: &[Host], down: bool) -> Result<String, String> {
    if hosts.is_empty() {
        return Err("no runner the target's selector picks is connected".into());
    }
    let project = crate::obj::name(o);
    let batch = o.pointer("/spec/batch").and_then(Value::as_u64).unwrap_or(1) as usize;
    let mut jobs = Vec::new();
    let mut before: Vec<String> = Vec::new();
    for group in runner::batches(hosts, if down { hosts.len() } else { batch }) {
        let mut ids = Vec::new();
        for h in group {
            let id = runner::job_id("compose", &h.name);
            jobs.push(Job { needs: before.clone(), checkout: false, timeout_minutes: 30, ..Job::script(id.clone(), format!("{project} {} on {}", if down { "down" } else { "up" }, h.name), json!(h.name), script(o, down)) });
            ids.push(id);
        }
        before = ids;
    }
    Ok(runner::workflow(&format!("kiln: {project} {}", if down { "down" } else { "up" }), &jobs))
}

pub fn health(o: &Value) -> Option<Status> {
    runner::health(o)
}
