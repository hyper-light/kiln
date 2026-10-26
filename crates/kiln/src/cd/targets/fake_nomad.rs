//! A fake of Nomad's HTTP API for tests, in memory, behaving as Nomad documents it where kiln
//! depends on it: registering a changed job makes a new version, rolled out by a deployment when
//! the job has an update block; a deployment of tasks that don't start fails, and with
//! `AutoRevert` the job goes back to its last stable version; a revert registers an earlier
//! version again; HCL is parsed by the API. Requests carry `X-Nomad-Token`.

use std::collections::BTreeMap;
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::http::server::{self as srv, Request, Response};

/// The token the fake takes.
pub const TOKEN: &str = "nomad-test-token";

/// A job as the fake keeps it: its versions (each the job as registered, and whether it was
/// stable), and its deployments, newest first.
#[derive(Clone, Debug, Default)]
pub struct Job {
    pub versions: Vec<(Value, bool)>,
    pub deployments: Vec<Value>,
    /// Reads left before the newest deployment ends.
    pub busy: u32,
    pub stopped: bool,
}

#[derive(Default)]
pub struct NomadState {
    /// Jobs by namespace and ID.
    pub jobs: BTreeMap<(String, String), Job>,
    /// Every change, in order: `Register api v1`.
    pub calls: Vec<String>,
    pub next: u64,
}

#[derive(Default)]
pub struct FakeNomad {
    pub state: Mutex<NomadState>,
}

fn err(status: u16, message: &str) -> Response {
    srv::text(http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::BAD_REQUEST), message)
}

fn ok(v: Value) -> Response {
    srv::json(http::StatusCode::OK, &v)
}

fn query_of(q: Option<&str>) -> BTreeMap<String, String> {
    q.unwrap_or_default().split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_owned(), srv::decode(v))).collect()
}

/// What a job's spec is, versions aside.
fn spec_of(j: &Value) -> Value {
    let mut j = j.clone();
    if let Some(m) = j.as_object_mut() {
        for k in ["Version", "Stable", "Status", "JobModifyIndex", "SubmitTime"] {
            m.remove(k);
        }
    }
    j
}

/// A small HCL reading, enough for the tests' jobs: the job's ID, its group and task, count,
/// driver, image, and its update block.
fn parse_hcl(hcl: &str) -> Option<Value> {
    let quoted = |key: &str| hcl.lines().map(str::trim).find_map(|l| l.strip_prefix(key).map(str::trim_start).and_then(|r| r.strip_prefix('"')).and_then(|r| r.split_once('"')).map(|(v, _)| v.to_owned()));
    let assigned = |key: &str| hcl.lines().map(str::trim).find_map(|l| l.strip_prefix(key).map(str::trim_start).and_then(|r| r.strip_prefix('=')).map(|v| v.trim().trim_matches('"').to_owned()));
    let id = quoted("job")?;
    let group = quoted("group").unwrap_or_else(|| id.clone());
    let task = quoted("task").unwrap_or_else(|| group.clone());
    let count = assigned("count").and_then(|c| c.parse::<u64>().ok()).unwrap_or(1);
    let mut job = json!({ "ID": id, "Name": id, "Type": assigned("type").unwrap_or_else(|| "service".into()),
        "TaskGroups": [{ "Name": group, "Count": count, "Tasks": [{ "Name": task, "Driver": assigned("driver").unwrap_or_else(|| "docker".into()), "Config": { "image": assigned("image").unwrap_or_default() } }] }] });
    if hcl.contains("update {") {
        let revert = assigned("auto_revert").is_some_and(|v| v == "true");
        if let Some(m) = job.as_object_mut() {
            m.insert("Update".into(), json!({ "MaxParallel": 1, "AutoRevert": revert }));
        }
    }
    Some(job)
}

/// A read: the newest deployment goes on, and ends successful, or failed when its tasks' image
/// is broken, the job then reverting to its last stable version when its update block says so.
fn step(st: &mut NomadState, key: &(String, String)) {
    let n = st.next + 1;
    let Some(j) = st.jobs.get_mut(key) else { return };
    if j.busy == 0 {
        return;
    }
    j.busy -= 1;
    if j.busy > 0 {
        return;
    }
    let Some(latest) = j.versions.len().checked_sub(1) else { return };
    let broken = j.versions.get(latest).is_some_and(|(v, _)| v.to_string().contains("broken"));
    let revert = j.versions.get(latest).is_some_and(|(v, _)| v.pointer("/Update/AutoRevert").and_then(Value::as_bool) == Some(true));
    if let Some(d) = j.deployments.first_mut().and_then(Value::as_object_mut) {
        d.insert("Status".into(), json!(if broken { "failed" } else { "successful" }));
        d.insert("StatusDescription".into(), json!(if broken { "Failed due to unhealthy allocations" } else { "Deployment completed successfully" }));
    }
    if let Some(v) = j.versions.get_mut(latest) {
        v.1 = !broken;
    }
    if broken && revert {
        // Back to the last stable version: registered again, as a new one.
        if let Some(stable) = j.versions.iter().rev().find(|(_, s)| *s).map(|(v, _)| v.clone()) {
            j.versions.push((stable, true));
            let version = j.versions.len() - 1;
            j.deployments.insert(0, json!({ "ID": format!("d{n}"), "JobVersion": version, "Status": "successful", "StatusDescription": "Deployment completed successfully (auto-reverted)" }));
            st.next = n;
        }
    }
}

fn job_answer(ns: &str, j: &Job) -> Value {
    let Some(version) = j.versions.len().checked_sub(1) else { return Value::Null };
    let (spec, stable) = j.versions.get(version).cloned().unwrap_or_default();
    let mut v = spec;
    if let Some(m) = v.as_object_mut() {
        m.insert("Namespace".into(), json!(ns));
        m.insert("Version".into(), json!(version));
        m.insert("Stable".into(), json!(stable));
        m.insert("Status".into(), json!(if j.stopped { "dead" } else { "running" }));
        m.insert("Stop".into(), json!(j.stopped));
        m.entry("Priority").or_insert(json!(50));
        m.entry("Datacenters").or_insert(json!(["*"]));
    }
    v
}

/// Registers `spec` as `id`'s newest version, if it changed: a deployment rolls it out when the
/// job has an update block.
fn register(st: &mut NomadState, ns: &str, id: &str, spec: Value, why: &str) {
    st.next += 1;
    let n = st.next;
    let j = st.jobs.entry((ns.to_owned(), id.to_owned())).or_default();
    j.stopped = false;
    if j.versions.last().is_some_and(|(v, _)| spec_of(v) == spec_of(&spec)) {
        return;
    }
    let update = spec.get("Update").is_some();
    j.versions.push((spec, !update));
    let version = j.versions.len() - 1;
    if update {
        j.deployments.insert(0, json!({ "ID": format!("d{n}"), "JobVersion": version, "Status": "running", "StatusDescription": "Deployment is running" }));
        j.busy = 2;
    }
    st.calls.push(format!("{why} {id} v{version}"));
}

impl srv::Handler for FakeNomad {
    async fn handle(self: Arc<Self>, req: Request) -> Response {
        let (method, path, query) = (req.method().clone(), req.uri().path().to_owned(), query_of(req.uri().query()));
        let token = req.headers().get("x-nomad-token").and_then(|v| v.to_str().ok()).unwrap_or("").to_owned();
        let raw = srv::body_bytes(req.into_body(), 16 << 20).await.unwrap_or_default();
        if token != TOKEN {
            return err(403, "Permission denied");
        }
        let body: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
        let ns = query.get("namespace").cloned().unwrap_or_else(|| "default".into());
        let mut st = self.state.lock();
        let parts: Vec<String> = path.trim_start_matches('/').split('/').map(srv::decode).collect();
        let p: Vec<&str> = parts.iter().map(String::as_str).collect();
        match (method.as_str(), p.as_slice()) {
            ("GET", ["v1", "jobs"]) => ok(Value::Array(st.jobs.iter().filter(|((n, _), _)| ns == "*" || *n == ns).map(|((n, id), j)| json!({ "ID": id, "Namespace": n, "Type": j.versions.last().and_then(|(v, _)| v.get("Type").cloned()), "Status": if j.stopped { "dead" } else { "running" } })).collect())),
            ("POST", ["v1", "jobs", "parse"]) => match body.get("JobHCL").and_then(Value::as_str).and_then(parse_hcl) {
                Some(j) => ok(j),
                None => err(400, "error parsing: no job block"),
            },
            ("POST", ["v1", "jobs"]) => {
                let Some(job) = body.get("Job").cloned() else { return err(400, "a Job is required") };
                let Some(id) = job.get("ID").and_then(Value::as_str).map(str::to_owned) else { return err(400, "the job has no ID") };
                let jns = job.get("Namespace").and_then(Value::as_str).map_or(ns, str::to_owned);
                register(&mut st, &jns, &id, spec_of(&job), "Register");
                ok(json!({ "EvalID": format!("e{}", st.next), "JobModifyIndex": st.next }))
            }
            (_, ["v1", "job", id, rest @ ..]) => {
                let key = (ns.clone(), (*id).to_owned());
                if !st.jobs.contains_key(&key) {
                    return err(404, "job not found");
                }
                match (method.as_str(), rest) {
                    ("GET", []) => {
                        step(&mut st, &key);
                        ok(st.jobs.get(&key).map(|j| job_answer(&ns, j)).unwrap_or_default())
                    }
                    ("GET", ["deployment"]) => {
                        step(&mut st, &key);
                        ok(st.jobs.get(&key).and_then(|j| j.deployments.first().cloned()).unwrap_or(Value::Null))
                    }
                    ("GET", ["deployments"]) => {
                        step(&mut st, &key);
                        ok(Value::Array(st.jobs.get(&key).map(|j| j.deployments.clone()).unwrap_or_default()))
                    }
                    ("POST", ["revert"]) => {
                        let Some(v) = body.get("JobVersion").and_then(Value::as_u64) else { return err(400, "JobVersion is required") };
                        let Some(spec) = st.jobs.get(&key).and_then(|j| j.versions.get(v as usize)).map(|(s, _)| s.clone()) else { return err(400, "no such version") };
                        register(&mut st, &ns, id, spec, &format!("Revert to v{v}:"));
                        ok(json!({ "EvalID": format!("e{}", st.next) }))
                    }
                    ("DELETE", []) => {
                        if let Some(j) = st.jobs.get_mut(&key) {
                            j.stopped = true;
                        }
                        st.calls.push(format!("Stop {id}"));
                        ok(json!({ "EvalID": "stop" }))
                    }
                    _ => err(404, "the fake has no such job call"),
                }
            }
            _ => err(404, &format!("the fake has no {method} {path}")),
        }
    }
}

/// Starts the fake on a free port, and points kiln's Nomad at it: its base URL.
pub async fn start(fake: Arc<FakeNomad>) -> anyhow::Result<String> {
    let tcp = hyper_rt::tcp::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), 64).map_err(|e| anyhow::anyhow!("{e}"))?;
    let addr = tcp.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
    crate::rt::spawn(async move {
        while let Ok(s) = tcp.accept().await {
            let (h, r) = (Arc::clone(&fake), s.peer_addr().ok());
            crate::rt::spawn(srv::serve_connection(crate::http::io::RtIo::new(s), h, r));
        }
    });
    let base = format!("http://{addr}");
    crate::ci::integrations::override_endpoint("nomad", &base);
    Ok(base)
}
