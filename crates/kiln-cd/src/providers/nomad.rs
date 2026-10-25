//! `nomad` (docs/cd-targets.md §3): a Nomad job, each release registered as a version of it and
//! rolled out by its `update` block (rolling, canaries, auto-promote, auto-revert).
//!
//! ```yaml
//! api:
//!   nomad:
//!     job: deploy/api.nomad.hcl                   # the job: HCL or JSON in the source, or an object
//!     set: { "TaskGroups[Name=api].Tasks[Name=api].Config.image": "ghcr.io/acme/api@${{ release.artifacts.image.digest }}" }
//! ```
//!
//! - **Job** (`api`, in its namespace): `spec.job` is the job as Nomad's API spells it (JSON:
//!   `ID`, `TaskGroups`, `Update`, …), or `spec.hcl` its HCL, which Nomad itself parses. `set:`
//!   applies to the JSON (for HCL, after Nomad parses it). Each release registers the job, a new
//!   version Nomad rolls out by its update block; kiln waits for the deployment. A rollback to a
//!   release kiln registered before reverts the job to that version.
//! - **Health** is the latest deployment's: successful, running, paused (canaries waiting to be
//!   promoted), or failed; and the job's status.

use serde_json::{Map, Value, json};

use super::{RenderInput, apply_sets, object, status, text};
use crate::health::Status;
use crate::model::Health;

pub const GROUP: &str = "nomad.kiln.dev";

/// The files a render reads.
pub fn files(spec: &Value) -> Vec<String> {
    super::file_settings(spec, &["job"])
}

/// A job's ID from its HCL: the label of its `job` block.
pub fn hcl_id(hcl: &str) -> Option<String> {
    hcl.lines().map(str::trim).find_map(|l| l.strip_prefix("job").map(str::trim_start).and_then(|r| r.strip_prefix('"')).and_then(|r| r.split_once('"')).map(|(id, _)| id.to_owned()))
}

/// An app's objects.
pub fn render(input: &RenderInput<'_>) -> Result<Vec<Value>, String> {
    let mut spec = Map::new();
    let (id, namespace) = match input.spec.get("job") {
        Some(Value::String(path)) if !path.ends_with(".json") => {
            let bytes = (input.read)(path).ok_or_else(|| format!("job: no file {path} in the source"))?;
            let hcl = String::from_utf8_lossy(&bytes).into_owned();
            let id = hcl_id(&hcl).ok_or_else(|| format!("{path}: no job \"<name>\" block"))?;
            spec.insert("hcl".into(), json!(hcl));
            if let Some(Value::Object(sets)) = input.spec.get("set") {
                spec.insert("set".into(), Value::Object(sets.clone()));
            }
            (id, text(input.spec, "namespace").unwrap_or("default").to_owned())
        }
        Some(_) => {
            let mut job = super::file_or_object(input, "job")?.ok_or("job: the job")?;
            // A job file as `nomad job inspect` writes it holds the job under `Job`.
            if let Some(inner) = job.get("Job").filter(|j| j.is_object()).cloned() {
                job = inner;
            }
            apply_sets(&mut job, input.spec.get("set"))?;
            let id = job.get("ID").and_then(Value::as_str).ok_or("the job has no ID")?.to_owned();
            let ns = job.get("Namespace").and_then(Value::as_str).or_else(|| text(input.spec, "namespace")).unwrap_or("default").to_owned();
            if let Some(m) = job.as_object_mut() {
                m.insert("Namespace".into(), json!(ns));
            }
            spec.insert("job".into(), job);
            (id, ns)
        }
        None => return Err("nomad needs job: the job's HCL or JSON in the source, or an object".into()),
    };
    Ok(vec![object(GROUP, "Job", &id, Some(&namespace), Value::Object(spec))])
}

/// A Job object from Nomad's job and its latest deployment. While the job is at the version
/// kiln's last apply registered (`ours`), it reads as kiln applied it.
pub fn live_job(job: &Value, deployment: Option<&Value>, applied: Option<&Value>, ours: bool) -> Option<Value> {
    let id = job.get("ID")?.as_str()?;
    let ns = job.get("Namespace").and_then(Value::as_str).unwrap_or("default");
    let spec = match (applied.and_then(|a| a.get("spec")), ours) {
        (Some(s), true) => s.clone(),
        _ => json!({ "job": job }),
    };
    let mut o = object(GROUP, "Job", id, Some(ns), spec);
    let d = deployment.filter(|d| !d.is_null()).map(|d| json!({ "id": d.get("ID"), "status": d.get("Status"), "description": d.get("StatusDescription"), "jobVersion": d.get("JobVersion") }));
    if let Value::Object(m) = &mut o {
        m.insert("status".into(), json!({ "status": job.get("Status"), "stop": job.get("Stop"), "version": job.get("Version"), "stable": job.get("Stable"), "deployment": d }));
    }
    Some(o)
}

pub fn health(o: &Value) -> Option<Status> {
    if crate::obj::kind(o) != "Job" {
        return None;
    }
    let st = o.get("status")?;
    let s = |p: &str| st.pointer(p).and_then(Value::as_str).unwrap_or_default().to_owned();
    if st.get("stop").and_then(Value::as_bool) == Some(true) {
        return status(Health::Degraded, "the job was stopped");
    }
    if let Some(d) = st.get("deployment").filter(|d| !d.is_null())
        && d.get("jobVersion") == st.get("version")
    {
        return match s("/deployment/status").as_str() {
            "successful" => status(Health::Healthy, ""),
            "running" | "pending" | "initializing" => status(Health::Progressing, s("/deployment/description")),
            "paused" | "blocked" => status(Health::Suspended, s("/deployment/description")),
            other => status(Health::Degraded, format!("its deployment {other}: {}", s("/deployment/description"))),
        };
    }
    match s("/status").as_str() {
        "running" => status(Health::Healthy, ""),
        "pending" => status(Health::Progressing, "pending"),
        "dead" => status(Health::Degraded, "the job is dead"),
        _ => None,
    }
}
