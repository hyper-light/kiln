//! A fake of Google Cloud's APIs for tests (Cloud Run, Cloud Storage), in memory, behaving as
//! Google documents them where kiln depends on it: a change is a long-running operation; a
//! revision starts before traffic moves to it, and traffic asked of one that fails to start
//! stays where it was; a revision named again with other content is refused; a change made at an
//! old etag is refused. Every request must carry the token, but uploads to a URL Google signed.

use std::collections::BTreeMap;
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use crate::http::server::{self as srv, Request, Response};

/// The token the fake takes.
pub const TOKEN: &str = "gcp-test-token";

/// Tests on the fake run one at a time: the endpoints they point at it are the process's.
pub static GCP: Mutex<()> = Mutex::new(());

/// A Cloud Run revision as the fake keeps it.
#[derive(Clone, Debug, Default)]
pub struct Revision {
    pub template: Value,
    /// Reads left before it has started, or failed to.
    pub busy: u32,
    pub ready: bool,
    pub failed: Option<String>,
}

/// A Cloud Run service as the fake keeps it.
#[derive(Clone, Debug, Default)]
pub struct RunService {
    /// What was set: template, traffic, labels, annotations, ingress, ….
    pub body: Map<String, Value>,
    pub generation: u64,
    pub revisions: BTreeMap<String, Revision>,
    /// Its revisions, oldest first.
    pub order: Vec<String>,
    pub latest_ready: String,
    /// The traffic as it's served.
    pub served: Vec<Value>,
    /// Why the traffic asked for can't be served, while it can't.
    pub routes_failed: Option<String>,
}

/// A long-running operation: what it's about (`run/<service>`), and how it ended.
#[derive(Clone, Debug, Default)]
pub struct Op {
    pub about: String,
    pub done: bool,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct GcpState {
    pub run: BTreeMap<String, RunService>,
    pub ops: BTreeMap<String, Op>,
    /// Cloud Storage: objects' bytes, by bucket and name.
    pub objects: BTreeMap<(String, String), Vec<u8>>,
    /// Every change, in order: `CreateService shop shop-k…`.
    pub calls: Vec<String>,
    pub next: u64,
}

#[derive(Default)]
pub struct FakeGcp {
    pub state: Mutex<GcpState>,
}

fn err(status: u16, code: &str, message: &str) -> Response {
    srv::json(http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::BAD_REQUEST), &json!({ "error": { "code": status, "message": message, "status": code } }))
}

fn ok(v: Value) -> Response {
    srv::json(http::StatusCode::OK, &v)
}

fn query_of(q: Option<&str>) -> BTreeMap<String, String> {
    q.unwrap_or_default().split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_owned(), srv::decode(v))).collect()
}

/// A run of the fake's (a revision's suffix, an operation's id).
fn next(st: &mut GcpState) -> u64 {
    st.next += 1;
    st.next
}

impl srv::Handler for FakeGcp {
    async fn handle(self: Arc<Self>, req: Request) -> Response {
        let (method, path, query) = (req.method().clone(), req.uri().path().to_owned(), query_of(req.uri().query()));
        let auth = srv::header_str(&req, &http::header::AUTHORIZATION).unwrap_or("").to_owned();
        let raw = srv::body_bytes(req.into_body(), 64 << 20).await.unwrap_or_default();
        if auth != format!("Bearer {TOKEN}") {
            return err(401, "UNAUTHENTICATED", "the request has no valid token");
        }
        let mut st = self.state.lock();
        if let Some(rest) = path.strip_prefix("/upload/storage/v1/b/") {
            let bucket = rest.split('/').next().unwrap_or_default().to_owned();
            let name = query.get("name").cloned().unwrap_or_default();
            if method != http::Method::POST || query.get("uploadType").map(String::as_str) != Some("media") || name.is_empty() {
                return err(400, "INVALID_ARGUMENT", "a media upload names its object");
            }
            st.calls.push(format!("UploadObject {bucket}/{name}"));
            let size = raw.len();
            st.objects.insert((bucket.clone(), name.clone()), raw.to_vec());
            return ok(json!({ "bucket": bucket, "name": name, "size": size.to_string(), "generation": "1" }));
        }
        if let Some(rest) = path.strip_prefix("/storage/v1/b/") {
            let parts: Vec<String> = rest.split('/').map(srv::decode).collect();
            return match parts.as_slice() {
                [bucket, o, name] if o == "o" && method == http::Method::GET => match st.objects.get(&(bucket.clone(), name.clone())) {
                    Some(b) => ok(json!({ "bucket": bucket, "name": name, "size": b.len().to_string() })),
                    None => err(404, "NOT_FOUND", &format!("No such object: {bucket}/{name}")),
                },
                _ => err(404, "NOT_FOUND", "the fake has no such storage call"),
            };
        }
        let body: Value = if raw.is_empty() { Value::Null } else { serde_json::from_slice(&raw).unwrap_or(Value::Null) };
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        match parts.as_slice() {
            ["v2", "projects", p, "locations", l, "operations", id] => operation(&mut st, &format!("projects/{p}/locations/{l}/operations/{id}")),
            ["v2", "projects", p, "locations", l, "services", rest @ ..] => run(&mut st, &method, p, l, rest, &query, &body),
            _ => err(404, "NOT_FOUND", &format!("the fake has no {method} {path}")),
        }
    }
}

/// An operation, moving what it's about along first.
fn operation(st: &mut GcpState, name: &str) -> Response {
    let Some(op) = st.ops.get(name).cloned() else { return err(404, "NOT_FOUND", &format!("no operation {name}")) };
    let mut op = op;
    if !op.done
        && let Some(s) = op.about.strip_prefix("run/")
    {
        match st.run.get_mut(s) {
            Some(svc) => {
                step(svc);
                if !busy(svc) {
                    op.done = true;
                    op.error = svc.routes_failed.clone();
                }
            }
            None => op.done = true,
        }
        st.ops.insert(name.to_owned(), op.clone());
    }
    let mut v = json!({ "name": name, "done": op.done });
    if let (Some(e), Value::Object(m)) = (&op.error, &mut v) {
        m.insert("error".into(), json!({ "code": 9, "message": e }));
    }
    ok(v)
}

fn new_op(st: &mut GcpState, prefix: &str, about: String, done: bool) -> Value {
    let id = next(st);
    let name = format!("{prefix}/operations/op-{id}");
    st.ops.insert(name.clone(), Op { about, done, error: None });
    json!({ "name": name, "done": done })
}

// ------------------------------------------------------------------ Cloud Run

/// A template with Cloud Run's defaults filled in, as it keeps them.
fn defaulted(t: &Value) -> Value {
    let mut t = t.clone();
    if let Some(m) = t.as_object_mut() {
        m.entry("maxInstanceRequestConcurrency").or_insert(json!(80));
        m.entry("timeout").or_insert(json!("300s"));
        if let Some(Value::Array(cs)) = m.get_mut("containers") {
            for c in cs.iter_mut().filter_map(Value::as_object_mut) {
                c.entry("ports").or_insert(json!([{ "name": "http1", "containerPort": 8080 }]));
                let r = c.entry("resources").or_insert(json!({}));
                if let Some(r) = r.as_object_mut() {
                    r.entry("limits").or_insert(json!({ "cpu": "1000m", "memory": "512Mi" }));
                    r.entry("cpuIdle").or_insert(json!(true));
                }
            }
        }
    }
    t
}

fn without_revision(t: &Value) -> Value {
    let mut t = t.clone();
    if let Some(m) = t.as_object_mut() {
        m.remove("revision");
    }
    t
}

fn busy(svc: &RunService) -> bool {
    svc.revisions.values().any(|r| r.busy > 0)
}

fn pct(t: &Value) -> u64 {
    t.get("percent").and_then(Value::as_u64).unwrap_or(0)
}

/// A read: revisions starting go on, and once none is, the traffic moves as asked if every
/// revision it goes to is ready.
fn step(svc: &mut RunService) {
    for r in svc.revisions.values_mut().filter(|r| r.busy > 0) {
        r.busy -= 1;
        if r.busy == 0 {
            let image = r.template.pointer("/containers/0/image").and_then(Value::as_str).unwrap_or_default();
            if image.contains("broken") {
                r.failed = Some("The user-provided container failed to start and listen on the port defined provided by the PORT=8080 environment variable.".into());
            } else {
                r.ready = true;
            }
        }
    }
    if busy(svc) {
        return;
    }
    if let Some(newest) = svc.order.iter().rev().find(|n| svc.revisions.get(*n).is_some_and(|r| r.ready)) {
        svc.latest_ready = newest.clone();
    }
    let traffic = svc.body.get("traffic").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut unready = None;
    for t in traffic.iter().filter(|t| pct(t) > 0) {
        let r = match t.get("type").and_then(Value::as_str) {
            Some("TRAFFIC_TARGET_ALLOCATION_TYPE_LATEST") => svc.latest_ready.clone(),
            _ => t.get("revision").and_then(Value::as_str).unwrap_or_default().to_owned(),
        };
        match svc.revisions.get(&r) {
            Some(rev) if rev.ready => {}
            Some(rev) => unready = Some(format!("Revision '{r}' is not ready and cannot serve traffic. {}", rev.failed.clone().unwrap_or_default())),
            None => unready = Some(format!("Revision '{r}' does not exist.")),
        }
    }
    match unready {
        Some(why) => svc.routes_failed = Some(why),
        None => {
            svc.routes_failed = None;
            svc.served = traffic;
        }
    }
}

fn run_answer(p: &str, l: &str, s: &str, svc: &RunService) -> Value {
    let full = |r: &str| format!("projects/{p}/locations/{l}/services/{s}/revisions/{r}");
    let reconciling = busy(svc);
    let latest_created = svc.order.last().cloned().unwrap_or_default();
    let config_failed = svc.revisions.get(&latest_created).and_then(|r| r.failed.clone()).map(|f| format!("Revision '{latest_created}' is not ready and cannot serve traffic. {f}"));
    let state = |failed: bool| if reconciling { "CONDITION_RECONCILING" } else if failed { "CONDITION_FAILED" } else { "CONDITION_SUCCEEDED" };
    let terminal = match (&svc.routes_failed, &config_failed) {
        (Some(why), _) | (None, Some(why)) => json!({ "type": "Ready", "state": state(true), "message": why }),
        (None, None) => json!({ "type": "Ready", "state": state(false) }),
    };
    let mut m = svc.body.clone();
    let served: Vec<Value> = svc.served.iter().map(|t| {
        let mut t = t.clone();
        if let (Some(tag), Value::Object(o)) = (t.get("tag").and_then(Value::as_str).map(str::to_owned), &mut t) {
            o.insert("uri".into(), json!(format!("https://{tag}---{s}-abc123-uc.a.run.app")));
        }
        t
    }).collect();
    let extra = json!({
        "name": format!("projects/{p}/locations/{l}/services/{s}"), "uid": format!("uid-{s}"), "generation": svc.generation.to_string(),
        "observedGeneration": (if reconciling { svc.generation.saturating_sub(1) } else { svc.generation }).to_string(), "etag": format!("\"{}\"", svc.generation),
        "uri": format!("https://{s}-abc123-uc.a.run.app"), "latestReadyRevision": full(&svc.latest_ready), "latestCreatedRevision": full(&latest_created),
        "trafficStatuses": served, "reconciling": reconciling, "terminalCondition": terminal,
        "conditions": [
            { "type": "RoutesReady", "state": state(svc.routes_failed.is_some()), "message": svc.routes_failed.clone().unwrap_or_default() },
            { "type": "ConfigurationsReady", "state": state(config_failed.is_some()), "message": config_failed.clone().unwrap_or_default() },
        ],
    });
    for (k, v) in extra.as_object().into_iter().flatten() {
        m.insert(k.clone(), v.clone());
    }
    Value::Object(m)
}

/// Sets what a create or an update asks of a service: a revision for a new template (unless it's
/// the template the service has), and the traffic.
fn set_service(st: &mut GcpState, s: &str, body: &Value, svc: &mut RunService) -> Result<(), Response> {
    let t = body.get("template").ok_or_else(|| err(400, "INVALID_ARGUMENT", "a service needs a template"))?;
    let template = defaulted(t);
    let current = svc.body.get("template").cloned().unwrap_or(Value::Null);
    let asked = template.get("revision").and_then(Value::as_str).map(str::to_owned);
    let same = without_revision(&template) == without_revision(&current) && asked.as_deref().is_none_or(|a| Some(a) == current.get("revision").and_then(Value::as_str));
    let mut made = None;
    if !same {
        let name = match asked {
            Some(a) => a,
            None => format!("{s}-{:05}-x{}", svc.generation + 1, next(st)),
        };
        if !name.starts_with(&format!("{s}-")) || name.len() > 63 {
            return Err(err(400, "INVALID_ARGUMENT", &format!("revision name {name} must start with {s}- and hold at most 63 characters")));
        }
        if svc.revisions.contains_key(&name) {
            return Err(err(409, "ALREADY_EXISTS", &format!("Revision named '{name}' with different configuration already exists.")));
        }
        made = Some(name);
    }
    let traffic = body.get("traffic").and_then(Value::as_array).cloned().unwrap_or_else(|| vec![json!({ "type": "TRAFFIC_TARGET_ALLOCATION_TYPE_LATEST", "percent": 100 })]);
    if traffic.iter().map(pct).sum::<u64>() != 100 {
        return Err(err(400, "INVALID_ARGUMENT", "the traffic's percents must add up to 100"));
    }
    for t in &traffic {
        if let Some(r) = t.get("revision").and_then(Value::as_str).filter(|r| !r.is_empty())
            && !svc.revisions.contains_key(r)
            && made.as_deref() != Some(r)
        {
            return Err(err(400, "INVALID_ARGUMENT", &format!("Revision '{r}' does not exist.")));
        }
    }
    let mut stored = body.as_object().cloned().unwrap_or_default();
    for k in kiln_cd::providers::gcp_run::OUTPUT {
        stored.remove(*k);
    }
    let mut template = template;
    if let Some(name) = &made {
        if let Some(m) = template.as_object_mut() {
            m.insert("revision".into(), json!(name));
        }
        svc.revisions.insert(name.clone(), Revision { template: template.clone(), busy: 2, ready: false, failed: None });
        svc.order.push(name.clone());
    } else {
        template = current;
    }
    stored.insert("template".into(), template);
    stored.insert("traffic".into(), Value::Array(traffic.clone()));
    svc.body = stored;
    svc.generation += 1;
    let moved: Vec<String> = traffic.iter().map(|t| format!("{}:{}", t.get("revision").and_then(Value::as_str).unwrap_or("latest"), pct(t))).collect();
    st.calls.push(format!("{} {s}{} traffic={}", if svc.generation == 1 { "CreateService" } else { "UpdateService" }, made.map(|m| format!(" revision={m}")).unwrap_or_default(), moved.join(",")));
    Ok(())
}

fn run(st: &mut GcpState, method: &http::Method, p: &str, l: &str, rest: &[&str], query: &BTreeMap<String, String>, body: &Value) -> Response {
    let prefix = format!("projects/{p}/locations/{l}");
    match (method.as_str(), rest) {
        ("GET", []) => ok(json!({ "services": st.run.iter().map(|(s, svc)| run_answer(p, l, s, svc)).collect::<Vec<_>>() })),
        ("POST", []) => {
            let s = query.get("serviceId").cloned().unwrap_or_default();
            if !kiln_cd::providers::gcp_run::valid_name(&s, 49) {
                return err(400, "INVALID_ARGUMENT", &format!("service id `{s}` isn't a name Cloud Run takes"));
            }
            if st.run.contains_key(&s) {
                return err(409, "ALREADY_EXISTS", &format!("Resource '{s}' already exists."));
            }
            let mut svc = RunService::default();
            if let Err(e) = set_service(st, &s, body, &mut svc) {
                return e;
            }
            st.run.insert(s.clone(), svc);
            ok(new_op(st, &prefix, format!("run/{s}"), false))
        }
        ("GET", [s]) => match st.run.get_mut(*s) {
            Some(svc) => {
                step(svc);
                ok(run_answer(p, l, s, svc))
            }
            None => err(404, "NOT_FOUND", &format!("Resource '{s}' of kind 'SERVICE' in region '{l}' in project '{p}' does not exist.")),
        },
        ("PATCH", [s]) => {
            let Some(mut svc) = st.run.get(*s).cloned() else { return err(404, "NOT_FOUND", &format!("Resource '{s}' does not exist.")) };
            if let Some(e) = body.get("etag").and_then(Value::as_str)
                && e != format!("\"{}\"", svc.generation)
            {
                return err(409, "ABORTED", "the service changed since its etag was read");
            }
            if let Err(e) = set_service(st, s, body, &mut svc) {
                return e;
            }
            st.run.insert((*s).to_owned(), svc);
            ok(new_op(st, &prefix, format!("run/{s}"), false))
        }
        ("DELETE", [s]) => match st.run.remove(*s) {
            Some(_) => {
                st.calls.push(format!("DeleteService {s}"));
                ok(new_op(st, &prefix, String::new(), true))
            }
            None => err(404, "NOT_FOUND", &format!("Resource '{s}' does not exist.")),
        },
        ("GET", [s, "revisions", r]) => match st.run.get_mut(*s) {
            Some(svc) => {
                step(svc);
                match svc.revisions.get(*r) {
                    Some(rev) => {
                        let state = if rev.busy > 0 { "CONDITION_RECONCILING" } else if rev.ready { "CONDITION_SUCCEEDED" } else { "CONDITION_FAILED" };
                        ok(json!({ "name": format!("{prefix}/services/{s}/revisions/{r}"), "containers": rev.template.get("containers"), "reconciling": rev.busy > 0,
                            "conditions": [{ "type": "Ready", "state": state, "message": rev.failed.clone().unwrap_or_default() }] }))
                    }
                    None => err(404, "NOT_FOUND", &format!("Revision '{r}' does not exist.")),
                }
            }
            None => err(404, "NOT_FOUND", &format!("Resource '{s}' does not exist.")),
        },
        _ => err(404, "NOT_FOUND", &format!("the fake has no {method} on services/{}", rest.join("/"))),
    }
}

impl GcpState {
    /// Someone moves a service's traffic in the console: all of it to the newest ready revision.
    pub fn follow_latest(&mut self, s: &str) {
        if let Some(svc) = self.run.get_mut(s) {
            svc.body.insert("traffic".into(), json!([{ "type": "TRAFFIC_TARGET_ALLOCATION_TYPE_LATEST", "percent": 100 }]));
            svc.generation += 1;
            step(svc);
        }
    }
}

/// Starts the fake on a free port, and points kiln's Google Cloud APIs at it: its base URL.
pub async fn start(fake: Arc<FakeGcp>) -> anyhow::Result<String> {
    let tcp = hyper_rt::tcp::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), 64).map_err(|e| anyhow::anyhow!("{e}"))?;
    let addr = tcp.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
    crate::rt::spawn(async move {
        while let Ok(s) = tcp.accept().await {
            let (h, r) = (Arc::clone(&fake), s.peer_addr().ok());
            crate::rt::spawn(srv::serve_connection(crate::http::io::RtIo::new(s), h, r));
        }
    });
    let base = format!("http://{addr}");
    for api in ["run", "cloudfunctions", "appengine", "compute", "storage"] {
        crate::ci::integrations::override_endpoint(&format!("gcp:{api}"), &base);
    }
    Ok(base)
}
