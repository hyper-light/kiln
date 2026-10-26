//! `microvm` (docs/cd-targets.md §3): a long-lived microVM (Firecracker) on each host kiln's
//! runners run on, each release's image booted beside the one before and taking its ports over
//! once healthy, the hosts a batch at a time.
//!
//! ```yaml
//! api:
//!   microvm:
//!     vm: api
//!     image: "ghcr.io/acme/api@${{ release.artifacts.image.digest }}"
//!     ports: ["8080:80"]                      # the host's ports that reach the microVM's
//!     health: http://127.0.0.1:8080/healthz   # a URL it answers, or a command run in there
//!     env: { MODE: prod }
//!     vcpus: 2
//!     memory: 1G
//!     batch: 1                                # hosts at a time (1)
//! ```
//!
//! - **VM** (`api`): `spec` is what each host runs: the image, its command, environment and
//!   ports, its health, and its size. kiln binds its runners' jobs to an environment of its own
//!   (`kiln-<vm>`, kind `microvm`, on the target's selector), where `kiln/deploy@v1` makes the
//!   deployment on the runner itself (W15-R02): the new microVM boots, passes its health, takes
//!   the ports over, and the one before stops: blue/green on each host. A batch starts once the one
//!   before passed. A rollback boots the earlier release's image the same way.
//! - **Health** is the newest run's.

use serde_json::{Map, Value, json};

use super::host::Host;
use super::runner::{self, Job, Step, plain};
use super::{RenderInput, object, text};
use crate::health::Status;

pub const GROUP: &str = "microvm.kiln.dev";

/// The files a render reads: none.
pub fn files(_spec: &Value) -> Vec<String> {
    Vec::new()
}

/// The environment a VM's jobs are bound to: `kiln-<vm>`.
pub fn environment(vm: &str) -> String {
    format!("kiln-{vm}")
}

fn lines(spec: &Value, key: &str) -> Result<Vec<String>, String> {
    match spec.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(s)) => Ok(s.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect()),
        Some(Value::Array(a)) => a.iter().map(|v| v.as_str().map(str::to_owned).ok_or_else(|| format!("{key}: a list of text"))).collect(),
        Some(_) => Err(format!("{key}: a list")),
    }
}

/// An app's objects.
pub fn render(input: &RenderInput<'_>) -> Result<Vec<Value>, String> {
    let vm = text(input.spec, "vm").ok_or("microvm needs vm: the microVM's name")?;
    if vm.len() > 40 || !vm.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') || vm.starts_with('-') {
        return Err(format!("vm `{vm}`: up to 40 lowercase letters, digits and -"));
    }
    let image = text(input.spec, "image").ok_or("microvm needs image: the image it boots (${{ release.artifacts.<name>.digest }})")?;
    let mut spec = Map::new();
    spec.insert("image".into(), json!(image));
    for k in ["command", "health"] {
        if let Some(v) = text(input.spec, k) {
            spec.insert(k.into(), json!(v));
        }
    }
    let ports = lines(input.spec, "ports")?;
    for p in &ports {
        if !p.split(':').all(|x| !x.is_empty()) || p.split(':').count() < 2 {
            return Err(format!("ports: `{p}` isn't host:guest (8080:80, or 127.0.0.1:8080:80)"));
        }
    }
    spec.insert("ports".into(), json!(ports));
    spec.insert("env".into(), Value::Object(runner::text_map(input.spec, "env")?));
    let size = |k: &str, d: &str| input.spec.get(k).map_or_else(|| d.to_owned(), |v| v.as_str().map_or_else(|| v.to_string(), str::to_owned));
    spec.insert("vcpus".into(), json!(size("vcpus", "1")));
    spec.insert("memory".into(), json!(size("memory", "512M")));
    let batch = input.spec.get("batch").and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))).unwrap_or(1).max(1);
    spec.insert("batch".into(), json!(batch));
    for (k, v) in &spec {
        if let Some(s) = v.as_str() {
            plain(k, s)?;
        }
    }
    Ok(vec![object(GROUP, "VM", vm, None, Value::Object(spec))])
}

/// The workflow that boots a VM's release on `hosts`, a batch at a time.
pub fn workflow(o: &Value, hosts: &[Host]) -> Result<String, String> {
    if hosts.is_empty() {
        return Err("no runner the target's selector picks is connected".into());
    }
    let vm = crate::obj::name(o);
    let env = environment(vm);
    let s = |k: &str| o.pointer(&format!("/spec/{k}")).and_then(Value::as_str).map(str::to_owned);
    let mut with = vec![("environment".to_owned(), env.clone()), ("image".to_owned(), s("image").unwrap_or_default())];
    for k in ["command", "health"] {
        if let Some(v) = s(k) {
            with.push((k.to_owned(), v));
        }
    }
    let envs: Vec<String> = o.pointer("/spec/env").and_then(Value::as_object).into_iter().flatten().map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default())).collect();
    if !envs.is_empty() {
        with.push(("env".to_owned(), format!("{}\n", envs.join("\n"))));
    }
    let ports: Vec<String> = o.pointer("/spec/ports").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
    if !ports.is_empty() {
        with.push(("ports".to_owned(), format!("{}\n", ports.join("\n"))));
    }
    with.push(("vcpus".to_owned(), s("vcpus").unwrap_or_else(|| "1".into())));
    with.push(("memory".to_owned(), s("memory").unwrap_or_else(|| "512M".into())));
    let batch = o.pointer("/spec/batch").and_then(Value::as_u64).unwrap_or(1) as usize;
    let mut jobs = Vec::new();
    let mut before: Vec<String> = Vec::new();
    for group in runner::batches(hosts, batch) {
        let mut ids = Vec::new();
        for h in group {
            let id = runner::job_id("vm", &h.name);
            let step = Step::Uses { uses: "kiln/deploy@v1".into(), with: with.clone() };
            jobs.push(Job { needs: before.clone(), checkout: false, environment: Some(env.clone()), step, timeout_minutes: 30, ..Job::script(id.clone(), format!("{vm} on {}", h.name), json!(h.name), String::new()) });
            ids.push(id);
        }
        before = ids;
    }
    Ok(runner::workflow(&format!("kiln: {vm} on its hosts"), &jobs))
}

pub fn health(o: &Value) -> Option<Status> {
    runner::health(o)
}
