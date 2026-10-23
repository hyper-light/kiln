//! A fake of Google Cloud's APIs for tests (Cloud Run, Cloud Functions, App Engine, Compute
//! Engine, Cloud Storage), in memory, behaving as Google documents them where kiln depends on it: a change is a long-running
//! operation; a revision starts before traffic moves to it, and traffic asked of one that fails to
//! start stays where it was; a revision named again with other content is refused; a change made
//! at an old etag is refused; a function builds from an upload, and one whose build fails leaves
//! the revision before serving; an App Engine version is made from a zip in Cloud Storage, and one
//! with traffic, or the default service, is never deleted; an instance template never changes and
//! isn't deleted while a group uses it; a group replaces its instances one read at a time, and
//! instances of a broken image fail to start, which listErrors says. Every request must carry the token, but uploads to a URL Google
//! signed.

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

/// A Cloud Function as the fake keeps it.
#[derive(Clone, Debug, Default)]
pub struct Function {
    /// What was set: buildConfig, serviceConfig, labels, ….
    pub body: Map<String, Value>,
    pub state: String,
    /// Reads left before its build is done.
    pub busy: u32,
    pub messages: Vec<String>,
    /// Its revisions so far, and the one serving.
    pub revisions: u32,
    pub serving: String,
}

/// An App Engine version as the fake keeps it.
#[derive(Clone, Debug, Default)]
pub struct AeVersion {
    pub body: Value,
    pub serving: String,
}

/// An App Engine service as the fake keeps it: its traffic split and its versions.
#[derive(Clone, Debug, Default)]
pub struct AeService {
    pub split: Map<String, Value>,
    pub versions: BTreeMap<String, AeVersion>,
}

/// An instance of a managed group: its template, and whether it runs yet.
#[derive(Clone, Debug, Default)]
pub struct Instance {
    pub name: String,
    pub template: String,
    pub running: bool,
}

/// A managed instance group as the fake keeps it.
#[derive(Clone, Debug, Default)]
pub struct Igm {
    pub body: Map<String, Value>,
    pub instances: Vec<Instance>,
    pub fingerprint: u64,
    pub errors: Vec<Value>,
}

#[derive(Default)]
pub struct GcpState {
    /// The fake's own address, for the upload URLs it signs.
    pub base: String,
    pub run: BTreeMap<String, RunService>,
    pub functions: BTreeMap<String, Function>,
    pub appengine: BTreeMap<String, AeService>,
    /// Compute Engine: instance templates by name, and managed instance groups.
    pub templates: BTreeMap<String, Value>,
    pub groups: BTreeMap<String, Igm>,
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
        let mut st = self.state.lock();
        // An upload to a URL the fake signed: no token, its signature in the query.
        if let Some(rest) = path.strip_prefix("/signed/") {
            let Some((bucket, object)) = rest.split_once('/') else { return err(404, "NOT_FOUND", "no such upload") };
            if method != http::Method::PUT || query.get("Signature").map(String::as_str) != Some("fake") || !auth.is_empty() {
                return err(403, "PERMISSION_DENIED", "a signed upload is a PUT with its signature, and no token");
            }
            st.calls.push(format!("UploadSource {bucket}/{object}"));
            st.objects.insert((bucket.to_owned(), object.to_owned()), raw.to_vec());
            return ok(json!({}));
        }
        if auth != format!("Bearer {TOKEN}") {
            return err(401, "UNAUTHENTICATED", "the request has no valid token");
        }
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
            ["v2", "projects", p, "locations", l, "functions:generateUploadUrl"] if method == http::Method::POST => {
                let id = next(&mut st);
                let bucket = format!("gcf-v2-uploads-{p}.{l}.cloudfunctions.appspot.com");
                let object = format!("upload-{id}.zip");
                ok(json!({ "uploadUrl": format!("{}/signed/{bucket}/{object}?GoogleAccessId=gcf&Expires=9999999999&Signature=fake", st.base), "storageSource": { "bucket": bucket, "object": object } }))
            }
            ["v2", "projects", p, "locations", l, "functions", rest @ ..] => functions(&mut st, &method, p, l, rest, &query, &body),
            ["v1", "apps", p, "operations", id] => operation(&mut st, &format!("apps/{p}/operations/{id}")),
            ["compute", "v1", "projects", p, "global", "operations", id] | ["compute", "v1", "projects", p, "zones", _, "operations", id] => {
                let name = format!("compute/{p}/{id}");
                match st.ops.get_mut(&name) {
                    Some(op) => {
                        op.done = true;
                        let link = format!("https://www.googleapis.com/compute/v1/{}", path.trim_start_matches("/compute/v1/"));
                        ok(json!({ "kind": "compute#operation", "name": id, "status": "DONE", "selfLink": link }))
                    }
                    None => err(404, "notFound", &format!("The resource '{path}' was not found")),
                }
            }
            ["compute", "v1", "projects", p, "global", "instanceTemplates", rest @ ..] => compute_templates(&mut st, &method, p, rest, &body),
            ["compute", "v1", "projects", p, "zones", z, "instanceGroupManagers", rest @ ..] => compute_groups(&mut st, &method, p, z, rest, &body),
            ["v1", "apps", p, "services", rest @ ..] => appengine(&mut st, &method, p, rest, &query, &body),
            _ => err(404, "NOT_FOUND", &format!("the fake has no {method} {path}")),
        }
    }
}

/// An operation, moving what it's about along first.
fn operation(st: &mut GcpState, name: &str) -> Response {
    let Some(op) = st.ops.get(name).cloned() else { return err(404, "NOT_FOUND", &format!("no operation {name}")) };
    let mut op = op;
    if !op.done {
        if let Some(s) = op.about.strip_prefix("run/") {
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
        } else if op.about.starts_with("ae/") {
            // App Engine's: done when next read, as it ended (its error set when it began).
            op.done = true;
        } else if let Some(f) = op.about.strip_prefix("fn/") {
            match st.functions.get_mut(f) {
                Some(func) => {
                    build(&st.objects, func, f);
                    if func.busy == 0 {
                        op.done = true;
                        op.error = (func.state != "ACTIVE").then(|| format!("Build failed with status: FAILURE. {}", func.messages.join("; ")));
                    }
                }
                None => op.done = true,
            }
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

// ------------------------------------------------------------------ Cloud Functions

/// A read: a build goes on; done, the function serves its new revision, or failed and serves
/// the one before.
fn build(objects: &BTreeMap<(String, String), Vec<u8>>, f: &mut Function, name: &str) {
    if f.busy == 0 {
        return;
    }
    f.busy -= 1;
    if f.busy > 0 {
        return;
    }
    let src = f.body.get("buildConfig").and_then(|b| b.pointer("/source/storageSource")).cloned().unwrap_or(Value::Null);
    let key = (src.get("bucket").and_then(Value::as_str).unwrap_or_default().to_owned(), src.get("object").and_then(Value::as_str).unwrap_or_default().to_owned());
    match objects.get(&key) {
        None => {
            f.state = "FAILED".into();
            f.messages = vec![format!("Failed to retrieve function source code: no object {}/{}", key.0, key.1)];
        }
        Some(zip) if String::from_utf8_lossy(zip).contains("broken") => {
            f.state = "FAILED".into();
            f.messages = vec!["Build failed: npm ERR! missing script: build".into()];
        }
        Some(_) => {
            f.revisions += 1;
            f.serving = format!("{name}-{:05}-abc", f.revisions);
            f.state = "ACTIVE".into();
            f.messages.clear();
        }
    }
}

fn function_answer(p: &str, l: &str, name: &str, f: &Function) -> Value {
    let mut m = f.body.clone();
    m.insert("name".into(), json!(format!("projects/{p}/locations/{l}/functions/{name}")));
    m.insert("state".into(), json!(if f.busy > 0 { "DEPLOYING" } else { f.state.as_str() }));
    m.insert("url".into(), json!(format!("https://{l}-{p}.cloudfunctions.net/{name}")));
    m.insert("updateTime".into(), json!("2026-10-22T00:00:00Z"));
    if !f.messages.is_empty() {
        m.insert("stateMessages".into(), Value::Array(f.messages.iter().map(|x| json!({ "severity": "ERROR", "type": "BuildFailed", "message": x })).collect()));
    }
    if let Some(Value::Object(b)) = m.get_mut("buildConfig") {
        b.insert("build".into(), json!(format!("projects/{p}/locations/{l}/builds/b{}", f.revisions)));
        if let Some(s) = b.get_mut("source").and_then(|s| s.get_mut("storageSource")).and_then(Value::as_object_mut) {
            s.insert("generation".into(), json!("1"));
        }
    }
    let svc = m.entry("serviceConfig").or_insert_with(|| json!({}));
    if let Some(c) = svc.as_object_mut() {
        c.entry("availableMemory").or_insert(json!("256M"));
        c.entry("timeoutSeconds").or_insert(json!(60));
        c.entry("allTrafficOnLatestRevision").or_insert(json!(true));
        c.insert("service".into(), json!(format!("projects/{p}/locations/{l}/services/{name}")));
        c.insert("uri".into(), json!(format!("https://{name}-abc123-uc.a.run.app")));
        if !f.serving.is_empty() {
            c.insert("revision".into(), json!(f.serving));
        }
    }
    Value::Object(m)
}

fn functions(st: &mut GcpState, method: &http::Method, p: &str, l: &str, rest: &[&str], query: &BTreeMap<String, String>, body: &Value) -> Response {
    let prefix = format!("projects/{p}/locations/{l}");
    let deploy = |st: &mut GcpState, name: &str, f: &mut Function| -> Result<(), Response> {
        let b = body.as_object().ok_or_else(|| err(400, "INVALID_ARGUMENT", "a function"))?;
        if b.get("buildConfig").and_then(|c| c.get("runtime")).and_then(Value::as_str).is_none_or(str::is_empty) {
            return Err(err(400, "INVALID_ARGUMENT", "buildConfig.runtime is required"));
        }
        let src = b.get("buildConfig").and_then(|c| c.pointer("/source/storageSource")).ok_or_else(|| err(400, "INVALID_ARGUMENT", "buildConfig.source.storageSource is required"))?;
        let mut stored = b.clone();
        for k in ["state", "updateTime", "createTime", "url", "stateMessages", "name"] {
            stored.remove(k);
        }
        if let Some(Value::Object(c)) = stored.get_mut("buildConfig") {
            c.remove("build");
        }
        if let Some(Value::Object(c)) = stored.get_mut("serviceConfig") {
            for k in ["service", "uri", "revision"] {
                c.remove(k);
            }
        }
        st.calls.push(format!("{} {name} source={}/{}", if f.state.is_empty() { "CreateFunction" } else { "UpdateFunction" }, src.get("bucket").and_then(Value::as_str).unwrap_or_default(), src.get("object").and_then(Value::as_str).unwrap_or_default()));
        f.body = stored;
        f.busy = 2;
        if f.state.is_empty() {
            f.state = "DEPLOYING".into();
        }
        Ok(())
    };
    match (method.as_str(), rest) {
        ("GET", []) => ok(json!({ "functions": st.functions.iter().map(|(n, f)| function_answer(p, l, n, f)).collect::<Vec<_>>() })),
        ("POST", []) => {
            let name = query.get("functionId").cloned().unwrap_or_default();
            if st.functions.contains_key(&name) {
                return err(409, "ALREADY_EXISTS", &format!("Function {name} already exists"));
            }
            let mut f = Function::default();
            if let Err(e) = deploy(st, &name, &mut f) {
                return e;
            }
            st.functions.insert(name.clone(), f);
            ok(new_op(st, &prefix, format!("fn/{name}"), false))
        }
        ("GET", [name]) => {
            let objects = std::mem::take(&mut st.objects);
            let answer = st.functions.get_mut(*name).map(|f| {
                build(&objects, f, name);
                function_answer(p, l, name, f)
            });
            st.objects = objects;
            match answer {
                Some(a) => ok(a),
                None => err(404, "NOT_FOUND", &format!("Resource 'projects/{p}/locations/{l}/functions/{name}' was not found")),
            }
        }
        ("PATCH", [name]) => {
            let Some(mut f) = st.functions.get(*name).cloned() else { return err(404, "NOT_FOUND", &format!("function {name} was not found")) };
            if f.busy > 0 {
                return err(409, "FAILED_PRECONDITION", "An operation on function {name} is already in progress");
            }
            if let Err(e) = deploy(st, name, &mut f) {
                return e;
            }
            st.functions.insert((*name).to_owned(), f);
            ok(new_op(st, &prefix, format!("fn/{name}"), false))
        }
        ("DELETE", [name]) => match st.functions.remove(*name) {
            Some(_) => {
                st.calls.push(format!("DeleteFunction {name}"));
                ok(new_op(st, &prefix, String::new(), true))
            }
            None => err(404, "NOT_FOUND", &format!("function {name} was not found")),
        },
        _ => err(404, "NOT_FOUND", &format!("the fake has no {method} on functions/{}", rest.join("/"))),
    }
}

// ------------------------------------------------------------------ App Engine

fn ae_service(p: &str, s: &str, svc: &AeService) -> Value {
    json!({ "name": format!("apps/{p}/services/{s}"), "id": s, "split": { "allocations": svc.split, "shardBy": "IP" } })
}

fn appengine(st: &mut GcpState, method: &http::Method, p: &str, rest: &[&str], query: &BTreeMap<String, String>, body: &Value) -> Response {
    let prefix = format!("apps/{p}");
    let ae_op = |st: &mut GcpState, about: String, error: Option<String>| {
        let v = new_op(st, &prefix, about, false);
        if let (Some(e), Some(name)) = (error, v.get("name").and_then(Value::as_str)) {
            if let Some(o) = st.ops.get_mut(name) {
                o.error = Some(e);
            }
        }
        ok(v)
    };
    match (method.as_str(), rest) {
        ("GET", []) => ok(json!({ "services": st.appengine.iter().map(|(s, svc)| ae_service(p, s, svc)).collect::<Vec<_>>() })),
        ("GET", [s]) => match st.appengine.get(*s) {
            Some(svc) => ok(ae_service(p, s, svc)),
            None => err(404, "NOT_FOUND", &format!("Unable to retrieve service {s}")),
        },
        ("PATCH", [s]) => {
            if query.get("updateMask").map(String::as_str) != Some("split") {
                return err(400, "INVALID_ARGUMENT", "the fake changes only split");
            }
            let Some(svc) = st.appengine.get_mut(*s) else { return err(404, "NOT_FOUND", &format!("Unable to retrieve service {s}")) };
            let allocations = body.pointer("/split/allocations").and_then(Value::as_object).cloned().unwrap_or_default();
            if let Some(v) = allocations.keys().find(|v| !svc.versions.contains_key(*v)) {
                return err(400, "INVALID_ARGUMENT", &format!("Version {v} does not exist in service {s}"));
            }
            if (allocations.values().filter_map(Value::as_f64).sum::<f64>() - 1.0).abs() > 0.001 {
                return err(400, "INVALID_ARGUMENT", "the allocations must add up to 1");
            }
            let moved: Vec<String> = allocations.iter().map(|(v, w)| format!("{v}:{w}")).collect();
            svc.split = allocations;
            st.calls.push(format!("UpdateService {s} split={}", moved.join(",")));
            ae_op(st, format!("ae/{s}"), None)
        }
        ("DELETE", [s]) => {
            if *s == "default" {
                return err(400, "FAILED_PRECONDITION", "The default service cannot be deleted.");
            }
            match st.appengine.remove(*s) {
                Some(_) => {
                    st.calls.push(format!("DeleteService {s}"));
                    ae_op(st, format!("ae/{s}"), None)
                }
                None => err(404, "NOT_FOUND", &format!("Unable to retrieve service {s}")),
            }
        }
        ("POST", [s, "versions"]) => {
            let id = body.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
            if !kiln_cd::providers::gcp_run::valid_name(&id, 63) {
                return err(400, "INVALID_ARGUMENT", &format!("version id `{id}` isn't one App Engine takes"));
            }
            if st.appengine.get(*s).is_some_and(|svc| svc.versions.contains_key(&id)) {
                return err(409, "ALREADY_EXISTS", &format!("Version {id} already exists"));
            }
            let failed = match (body.pointer("/deployment/zip/sourceUrl").and_then(Value::as_str), body.pointer("/deployment/container/image").and_then(Value::as_str)) {
                (Some(url), _) => {
                    let place = url.strip_prefix("https://storage.googleapis.com/").and_then(|r| r.split_once('/')).map(|(b, o)| (b.to_owned(), o.to_owned()));
                    match place.and_then(|k| st.objects.get(&k)) {
                        None => Some(format!("Failed to fetch the source zip {url}")),
                        Some(z) if String::from_utf8_lossy(z).contains("broken") => Some("The app failed to start: Error: Server Error".to_owned()),
                        Some(_) => None,
                    }
                }
                (None, Some(i)) => i.contains("broken").then(|| "The app failed to start: Error: Server Error".to_owned()),
                (None, None) => return err(400, "INVALID_ARGUMENT", "a version needs its deployment"),
            };
            st.calls.push(format!("CreateVersion {s} {id}"));
            if failed.is_none() {
                let mut b = body.clone();
                if let Some(m) = b.as_object_mut() {
                    m.remove("id");
                }
                let svc = st.appengine.entry((*s).to_owned()).or_default();
                svc.versions.insert(id.clone(), AeVersion { body: b, serving: "SERVING".into() });
                // A new service sends its traffic to its first version.
                if svc.split.is_empty() {
                    svc.split.insert(id.clone(), json!(1));
                }
            }
            ae_op(st, format!("ae/{s}/{id}"), failed)
        }
        ("GET", [s, "versions", v]) => match st.appengine.get(*s).and_then(|svc| svc.versions.get(*v)) {
            Some(ver) => {
                let mut a = ver.body.as_object().cloned().unwrap_or_default();
                if query.get("view").map(String::as_str) != Some("FULL") {
                    a.remove("deployment");
                }
                a.insert("name".into(), json!(format!("apps/{p}/services/{s}/versions/{v}")));
                a.insert("id".into(), json!(v));
                a.insert("servingStatus".into(), json!(ver.serving));
                a.insert("versionUrl".into(), json!(format!("https://{v}-dot-{s}-dot-{p}.appspot.com")));
                a.insert("createTime".into(), json!("2026-10-22T00:00:00Z"));
                a.entry("instanceClass").or_insert(json!("F1"));
                ok(Value::Object(a))
            }
            None => err(404, "NOT_FOUND", &format!("Version {v} does not exist")),
        },
        ("DELETE", [s, "versions", v]) => {
            let Some(svc) = st.appengine.get_mut(*s) else { return err(404, "NOT_FOUND", &format!("Unable to retrieve service {s}")) };
            if svc.split.contains_key(*v) {
                return err(400, "FAILED_PRECONDITION", "Cannot delete a version with a non-zero traffic allocation. Migrate traffic away first.");
            }
            match svc.versions.remove(*v) {
                Some(_) => {
                    st.calls.push(format!("DeleteVersion {s} {v}"));
                    ae_op(st, format!("ae/{s}/{v}"), None)
                }
                None => err(404, "NOT_FOUND", &format!("Version {v} does not exist")),
            }
        }
        _ => err(404, "NOT_FOUND", &format!("the fake has no {method} on services/{}", rest.join("/"))),
    }
}

// ------------------------------------------------------------------ Compute Engine

/// Compute Engine's error shape: `errors[].reason`, no `status`.
fn gce_err(status: u16, reason: &str, message: &str) -> Response {
    srv::json(http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::BAD_REQUEST), &json!({ "error": { "code": status, "message": message, "errors": [{ "domain": "global", "reason": reason, "message": message }] } }))
}

/// An operation in progress, done when next read.
fn gce_op(st: &mut GcpState, p: &str, scope: &str) -> Response {
    let id = format!("operation-{}", next(st));
    st.ops.insert(format!("compute/{p}/{id}"), Op { about: "gce".into(), done: false, error: None });
    ok(json!({ "kind": "compute#operation", "name": id, "status": "RUNNING", "selfLink": format!("https://www.googleapis.com/compute/v1/projects/{p}/{scope}/operations/{id}") }))
}

fn compute_templates(st: &mut GcpState, method: &http::Method, p: &str, rest: &[&str], body: &Value) -> Response {
    match (method.as_str(), rest) {
        ("POST", []) => {
            let name = body.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
            if st.templates.contains_key(&name) {
                return gce_err(409, "alreadyExists", &format!("The resource 'projects/{p}/global/instanceTemplates/{name}' already exists"));
            }
            if body.pointer("/properties/machineType").and_then(Value::as_str).is_none_or(str::is_empty) {
                return gce_err(400, "invalid", "Invalid value for field 'resource.properties.machineType'");
            }
            let mut t = body.clone();
            if let Some(m) = t.as_object_mut() {
                m.insert("selfLink".into(), json!(format!("https://www.googleapis.com/compute/v1/projects/{p}/global/instanceTemplates/{name}")));
                m.insert("creationTimestamp".into(), json!("2026-10-22T00:00:00.000-07:00"));
                // Compute Engine's defaults, filled in.
                if let Some(Value::Array(disks)) = m.get_mut("properties").and_then(|x| x.get_mut("disks")) {
                    for d in disks.iter_mut().filter_map(Value::as_object_mut) {
                        d.entry("type").or_insert(json!("PERSISTENT"));
                        d.entry("mode").or_insert(json!("READ_WRITE"));
                    }
                }
            }
            st.calls.push(format!("InsertTemplate {name}"));
            st.templates.insert(name, t);
            gce_op(st, p, "global")
        }
        ("GET", [name]) => match st.templates.get(*name) {
            Some(t) => ok(t.clone()),
            None => gce_err(404, "notFound", &format!("The resource 'projects/{p}/global/instanceTemplates/{name}' was not found")),
        },
        ("DELETE", [name]) => {
            if !st.templates.contains_key(*name) {
                return gce_err(404, "notFound", &format!("The resource 'projects/{p}/global/instanceTemplates/{name}' was not found"));
            }
            if let Some((g, _)) = st.groups.iter().find(|(_, g)| group_template(g) == *name) {
                return gce_err(400, "resourceInUseByAnotherResource", &format!("The instance_template resource '{name}' is already being used by '{g}'"));
            }
            st.templates.remove(*name);
            st.calls.push(format!("DeleteTemplate {name}"));
            gce_op(st, p, "global")
        }
        _ => gce_err(404, "notFound", "the fake has no such call on instance templates"),
    }
}

/// The template a group's version names.
fn group_template(g: &Igm) -> String {
    g.body.get("versions").and_then(|v| v.pointer("/0/instanceTemplate")).and_then(Value::as_str).map(|t| t.rsplit('/').next().unwrap_or(t).to_owned()).unwrap_or_default()
}

/// A read: an instance starting on the group's template starts, or else one off it is replaced by
/// one on it; with a broken image, the new instance fails to start, and listErrors says so.
fn roll(templates: &BTreeMap<String, Value>, g: &mut Igm, n: &mut u64) {
    let target = group_template(g);
    let broken = templates.get(&target).and_then(|t| t.pointer("/properties/disks/0/initializeParams/sourceImage")).and_then(Value::as_str).is_some_and(|i| i.contains("broken"));
    let now = time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).unwrap_or_default();
    let failed = |name: &str| json!({ "error": { "code": "RESOURCE_NOT_READY", "message": format!("Instance '{name}' failed to start: its boot disk's image doesn't boot") }, "timestamp": now });
    if let Some(i) = g.instances.iter_mut().find(|i| !i.running && i.template == target) {
        if broken {
            let e = failed(&i.name);
            g.errors.push(e);
        } else {
            i.running = true;
        }
        return;
    }
    if let Some(pos) = g.instances.iter().position(|i| i.template != target) {
        *n += 1;
        let name = format!("{}-{:04x}", g.body.get("baseInstanceName").and_then(Value::as_str).unwrap_or("vm"), *n);
        if broken {
            g.errors.push(failed(&name));
        } else if let Some(slot) = g.instances.get_mut(pos) {
            *slot = Instance { name, template: target, running: true };
        }
    }
}

fn group_answer(p: &str, z: &str, name: &str, g: &Igm) -> Value {
    let target = group_template(g);
    let stable = g.instances.iter().all(|i| i.running && i.template == target);
    let creating = g.instances.iter().filter(|i| !i.running).count();
    let replacing = g.instances.iter().filter(|i| i.template != target).count();
    let mut m = g.body.clone();
    let link = |t: &str| json!(format!("https://www.googleapis.com/compute/v1/projects/{p}/global/instanceTemplates/{t}"));
    if let Some(Value::Array(vs)) = m.get_mut("versions") {
        for v in vs.iter_mut().filter_map(Value::as_object_mut) {
            if let Some(t) = v.get("instanceTemplate").and_then(Value::as_str).map(|t| t.rsplit('/').next().unwrap_or(t).to_owned()) {
                v.insert("instanceTemplate".into(), link(&t));
            }
        }
    }
    if let Some(Value::Object(u)) = m.get_mut("updatePolicy") {
        u.entry("replacementMethod").or_insert(json!("SUBSTITUTE"));
        u.entry("maxUnavailable").or_insert(json!({ "fixed": 1, "calculated": 1 }));
    }
    m.insert("name".into(), json!(name));
    m.insert("zone".into(), json!(format!("https://www.googleapis.com/compute/v1/projects/{p}/zones/{z}")));
    m.insert("selfLink".into(), json!(format!("https://www.googleapis.com/compute/v1/projects/{p}/zones/{z}/instanceGroupManagers/{name}")));
    m.insert("instanceTemplate".into(), link(&target));
    m.insert("fingerprint".into(), json!(format!("fp{}", g.fingerprint)));
    m.insert("currentActions".into(), json!({ "none": g.instances.len() - creating.max(replacing).min(g.instances.len()), "creating": creating, "recreating": 0, "deleting": 0, "replacing": replacing }));
    m.insert("status".into(), json!({ "isStable": stable, "versionTarget": { "isReached": g.instances.iter().all(|i| i.template == target) } }));
    Value::Object(m)
}

fn compute_groups(st: &mut GcpState, method: &http::Method, p: &str, z: &str, rest: &[&str], body: &Value) -> Response {
    let scope = format!("zones/{z}");
    match (method.as_str(), rest) {
        ("GET", []) => ok(json!({ "items": st.groups.iter().map(|(n, g)| group_answer(p, z, n, g)).collect::<Vec<_>>() })),
        ("POST", []) => {
            let name = body.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
            if st.groups.contains_key(&name) {
                return gce_err(409, "alreadyExists", &format!("The resource 'projects/{p}/zones/{z}/instanceGroupManagers/{name}' already exists"));
            }
            let mut g = Igm { body: body.as_object().cloned().unwrap_or_default(), ..Igm::default() };
            g.body.remove("name");
            let t = group_template(&g);
            if !st.templates.contains_key(&t) {
                return gce_err(400, "invalid", &format!("Invalid value for field 'resource.versions[0].instanceTemplate': the template {t} does not exist"));
            }
            let size = g.body.get("targetSize").and_then(Value::as_u64).unwrap_or(0);
            for _ in 0..size {
                let n = next(st);
                g.instances.push(Instance { name: format!("{}-{n:04x}", g.body.get("baseInstanceName").and_then(Value::as_str).unwrap_or("vm")), template: t.clone(), running: false });
            }
            st.calls.push(format!("InsertGroup {name} template={t}"));
            st.groups.insert(name, g);
            gce_op(st, p, &scope)
        }
        ("GET", [name]) => {
            let templates = std::mem::take(&mut st.templates);
            let mut n = st.next;
            let answer = st.groups.get_mut(*name).map(|g| {
                roll(&templates, g, &mut n);
                group_answer(p, z, name, g)
            });
            st.next = n;
            st.templates = templates;
            match answer {
                Some(a) => ok(a),
                None => gce_err(404, "notFound", &format!("The resource 'projects/{p}/zones/{z}/instanceGroupManagers/{name}' was not found")),
            }
        }
        ("GET", [name, "listErrors"]) => match st.groups.get(*name) {
            Some(g) => ok(json!({ "items": g.errors })),
            None => gce_err(404, "notFound", "no such group"),
        },
        ("PATCH", [name]) => {
            let Some(g) = st.groups.get_mut(*name) else { return gce_err(404, "notFound", "no such group") };
            if body.get("fingerprint").and_then(Value::as_str).is_some_and(|f| f != format!("fp{}", g.fingerprint)) {
                return gce_err(412, "conditionNotMet", "Supplied fingerprint does not match current resource fingerprint.");
            }
            for (k, v) in body.as_object().into_iter().flatten().filter(|(k, _)| *k != "fingerprint") {
                g.body.insert(k.clone(), v.clone());
            }
            g.fingerprint += 1;
            let t = group_template(g);
            if !st.templates.contains_key(&t) {
                return gce_err(400, "invalid", &format!("the template {t} does not exist"));
            }
            st.calls.push(format!("PatchGroup {name} template={t}"));
            gce_op(st, p, &scope)
        }
        ("DELETE", [name]) => match st.groups.remove(*name) {
            Some(_) => {
                st.calls.push(format!("DeleteGroup {name}"));
                gce_op(st, p, &scope)
            }
            None => gce_err(404, "notFound", "no such group"),
        },
        _ => gce_err(404, "notFound", "the fake has no such call on instance groups"),
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

/// Whether `live` reads as `desired` says, as the engine compares them: what kiln sets.
pub fn synced(desired: &Value, live: &Value) -> bool {
    let mut d = desired.clone();
    kiln_cd::diff::normalize_desired(&mut d);
    matches!(kiln_cd::diff::compare(&d, live, None), kiln_cd::diff::Verdict::Synced)
}

/// The test project's target for `provider`, and its place as an environment names it.
pub fn target(provider: &str) -> anyhow::Result<(kiln_cd::providers::Target, Value)> {
    let def = kiln_cd::providers::get(provider).ok_or_else(|| anyhow::anyhow!("no provider {provider}"))?;
    let mut place = json!({ "account": "gcp-test", "project": "acme-prod" });
    for k in def.place.iter().filter(|k| **k != "project") {
        if let Value::Object(m) = &mut place {
            m.insert((*k).to_owned(), json!(if *k == "zone" { "us-central1-a" } else { "us-central1" }));
        }
    }
    let (t, _) = kiln_cd::providers::target_of(provider, place.as_object().ok_or_else(|| anyhow::anyhow!("place"))?).map_err(|e| anyhow::anyhow!("{e}"))?;
    anyhow::ensure!(def.built, "{provider} isn't built");
    Ok((t, place))
}

/// Starts the fake on a free port, and points kiln's Google Cloud APIs at it: its base URL.
pub async fn start(fake: Arc<FakeGcp>) -> anyhow::Result<String> {
    let tcp = hyper_rt::tcp::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), 64).map_err(|e| anyhow::anyhow!("{e}"))?;
    let addr = tcp.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
    let base = format!("http://{addr}");
    fake.state.lock().base.clone_from(&base);
    crate::rt::spawn(async move {
        while let Ok(s) = tcp.accept().await {
            let (h, r) = (Arc::clone(&fake), s.peer_addr().ok());
            crate::rt::spawn(srv::serve_connection(crate::http::io::RtIo::new(s), h, r));
        }
    });
    for api in ["run", "cloudfunctions", "appengine", "compute", "storage"] {
        crate::ci::integrations::override_endpoint(&format!("gcp:{api}"), &base);
    }
    Ok(base)
}
