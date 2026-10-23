//! A fake of the hosting platforms' APIs for tests (Render, DigitalOcean App Platform, Heroku), in
//! memory, behaving as each
//! documents it where kiln depends on it: a deploy builds and goes live, the one before serving
//! until it does, and one that fails leaves it serving; a rollback re-deploys an earlier deploy.
//! Every request carries the platform's token.

use std::collections::BTreeMap;
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::http::server::{self as srv, Request, Response};

/// The token the fake takes.
pub const TOKEN: &str = "hosting-test-token";

/// Tests on the fake run one at a time: the endpoints they point at it are the process's.
pub static HOSTING: Mutex<()> = Mutex::new(());

/// A Render service as the fake keeps it: its deploys, newest first.
#[derive(Clone, Debug, Default)]
pub struct RenderService {
    pub id: String,
    pub deploys: Vec<Value>,
    /// Reads left before the newest deploy is done.
    pub busy: u32,
}

/// An App Platform app as the fake keeps it: its spec, and its deployments, newest first.
#[derive(Clone, Debug, Default)]
pub struct DoApp {
    pub spec: Value,
    pub deployments: Vec<Value>,
    /// Reads left before the newest deployment is done.
    pub busy: u32,
}

/// A Heroku app as the fake keeps it: its releases and builds, newest first, its config vars, and
/// whether its dynos start on its current release (by slug).
#[derive(Clone, Debug, Default)]
pub struct HerokuApp {
    pub releases: Vec<Value>,
    pub builds: Vec<Value>,
    pub config: serde_json::Map<String, Value>,
    /// Reads left before the newest build is done; and before the dynos have started.
    pub building: u32,
    pub starting: u32,
    /// Slugs whose dynos crash.
    pub crashing: Vec<String>,
}

#[derive(Default)]
pub struct HostingState {
    pub base: String,
    pub heroku: BTreeMap<String, HerokuApp>,
    /// Places sources were sent to, by id.
    pub blobs: BTreeMap<String, Vec<u8>>,
    pub render: BTreeMap<String, RenderService>,
    /// App Platform's apps, by id.
    pub apps: BTreeMap<String, DoApp>,
    /// Every change, in order: `Deploy shop-web …`.
    pub calls: Vec<String>,
    pub next: u64,
}

#[derive(Default)]
pub struct FakeHosting {
    pub state: Mutex<HostingState>,
}

fn err(status: u16, code: &str, message: &str) -> Response {
    srv::json(http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::BAD_REQUEST), &json!({ "id": code, "message": message }))
}

fn ok(v: Value) -> Response {
    srv::json(http::StatusCode::OK, &v)
}

/// Sets a field of an object.
fn set(v: &mut Value, k: &str, x: Value) {
    if let Some(m) = v.as_object_mut() {
        m.insert(k.to_owned(), x);
    }
}

fn next(st: &mut HostingState) -> u64 {
    st.next += 1;
    st.next
}

fn query_of(q: Option<&str>) -> BTreeMap<String, String> {
    q.unwrap_or_default().split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_owned(), srv::decode(v))).collect()
}

impl srv::Handler for FakeHosting {
    async fn handle(self: Arc<Self>, req: Request) -> Response {
        let (method, path, query) = (req.method().clone(), req.uri().path().to_owned(), query_of(req.uri().query()));
        let auth = srv::header_str(&req, &http::header::AUTHORIZATION).unwrap_or("").to_owned();
        let accept = srv::header_str(&req, &http::header::ACCEPT).unwrap_or("").to_owned();
        let raw = srv::body_bytes(req.into_body(), 64 << 20).await.unwrap_or_default();
        let mut st = self.state.lock();
        // A place Heroku's sources signed: no token.
        if let Some(id) = path.strip_prefix("/blobs/") {
            if query.get("sig").map(String::as_str) != Some("put") || method != http::Method::PUT {
                return err(403, "forbidden", "a source's place takes a signed PUT");
            }
            st.calls.push("UploadSource".to_owned());
            st.blobs.insert(id.to_owned(), raw.to_vec());
            return ok(Value::Null);
        }
        if auth != format!("Bearer {TOKEN}") {
            return err(401, "unauthorized", "a valid token is required");
        }
        let body: Value = if raw.is_empty() { Value::Null } else { serde_json::from_slice(&raw).unwrap_or(Value::Null) };
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        match parts.as_slice() {
            ["v1", "services", rest @ ..] => render(&mut st, &method, rest, &query, &body),
            ["v2", "apps", rest @ ..] => digitalocean(&mut st, &method, rest, &body),
            ["apps", ..] | ["sources"] if !accept.contains("application/vnd.heroku+json; version=3") => err(406, "variant_not_available", "Heroku's API wants its Accept header"),
            ["sources"] if method == http::Method::POST => {
                let n = next(&mut st);
                let base = st.base.clone();
                ok(json!({ "source_blob": { "get_url": format!("{base}/blobs/s{n}?sig=get"), "put_url": format!("{base}/blobs/s{n}?sig=put") } }))
            }
            ["apps", rest @ ..] => heroku(&mut st, &method, rest, &body),
            _ => err(404, "not_found", &format!("the fake has no {method} {path}")),
        }
    }
}

// ------------------------------------------------------------------ Render

fn render_service(name: &str, s: &RenderService) -> Value {
    json!({ "id": s.id, "name": name, "type": "web_service", "suspended": "not_suspended", "updatedAt": "2026-10-23T00:00:00Z", "serviceDetails": { "url": format!("https://{name}.onrender.com") } })
}

/// A read: the newest deploy goes on; done, it's live and the one before deactivated, or it
/// failed and the one before serves still.
fn render_step(s: &mut RenderService) {
    if s.busy == 0 {
        return;
    }
    s.busy -= 1;
    let Some(newest) = s.deploys.first().cloned() else { return };
    if s.busy == 1 {
        if let Some(d) = s.deploys.first_mut() {
            set(d, "status", json!("update_in_progress"));
        }
        return;
    }
    let broken = newest.to_string().contains("broken");
    if !broken {
        for d in s.deploys.iter_mut().skip(1).filter(|d| d.get("status").and_then(Value::as_str) == Some("live")) {
            set(d, "status", json!("deactivated"));
        }
    }
    if let Some(d) = s.deploys.first_mut() {
        set(d, "status", json!(if broken { "update_failed" } else { "live" }));
    }
}

fn render(st: &mut HostingState, method: &http::Method, rest: &[&str], query: &BTreeMap<String, String>, body: &Value) -> Response {
    let by_id = |st: &HostingState, id: &str| st.render.iter().find(|(_, s)| s.id == id).map(|(n, _)| n.clone());
    match (method.as_str(), rest) {
        ("GET", []) => {
            let want = query.get("name");
            ok(Value::Array(st.render.iter().filter(|(n, _)| want.is_none_or(|w| w == *n)).map(|(n, s)| json!({ "cursor": "c", "service": render_service(n, s) })).collect()))
        }
        (_, [id, rest @ ..]) => {
            let Some(name) = by_id(st, id) else { return err(404, "not_found", "Service not found") };
            let n = next(st);
            let Some(s) = st.render.get_mut(&name) else { return err(404, "not_found", "Service not found") };
            match (method.as_str(), rest) {
                ("GET", ["deploys"]) => {
                    render_step(s);
                    ok(Value::Array(s.deploys.iter().map(|d| json!({ "cursor": "c", "deploy": d })).collect()))
                }
                ("GET", ["deploys", did]) => {
                    render_step(s);
                    match s.deploys.iter().find(|d| d.get("id").and_then(Value::as_str) == Some(*did)) {
                        Some(d) => ok(d.clone()),
                        None => err(404, "not_found", "Deploy not found"),
                    }
                }
                ("POST", ["deploys"]) => {
                    if s.busy > 0 {
                        return err(409, "conflict", "a deploy is in progress");
                    }
                    let mut d = json!({ "id": format!("dep-{n}"), "status": "build_in_progress", "trigger": "api", "createdAt": "2026-10-23T00:00:00Z" });
                    let what = match (body.get("imageUrl").and_then(Value::as_str), body.get("commitId").and_then(Value::as_str)) {
                        (Some(i), _) => {
                            set(&mut d, "image", json!({ "ref": i, "sha": format!("sha256:{n:064x}") }));
                            i.to_owned()
                        }
                        (None, Some(c)) => {
                            set(&mut d, "commit", json!({ "id": c }));
                            c.to_owned()
                        }
                        (None, None) => return err(400, "bad_request", "imageUrl or commitId"),
                    };
                    s.deploys.insert(0, d.clone());
                    s.busy = 2;
                    st.calls.push(format!("Deploy {name} {what}"));
                    srv::json(http::StatusCode::CREATED, &d)
                }
                ("POST", ["rollback"]) => {
                    let target = body.get("deployId").and_then(Value::as_str).unwrap_or_default();
                    let Some(before) = s.deploys.iter().find(|d| d.get("id").and_then(Value::as_str) == Some(target)).cloned() else { return err(404, "not_found", "Deploy not found") };
                    let mut d = before.clone();
                    set(&mut d, "id", json!(format!("dep-{n}")));
                    set(&mut d, "status", json!("update_in_progress"));
                    set(&mut d, "trigger", json!("rollback"));
                    s.deploys.insert(0, d.clone());
                    s.busy = 1;
                    st.calls.push(format!("Rollback {name} {target}"));
                    srv::json(http::StatusCode::CREATED, &d)
                }
                _ => err(404, "not_found", "the fake has no such Render call"),
            }
        }
        _ => err(404, "not_found", "the fake has no such Render call"),
    }
}

// ------------------------------------------------------------------ DigitalOcean App Platform

/// App Platform's defaults, filled into a spec as it keeps it.
fn do_defaulted(spec: &Value) -> Value {
    let mut s = spec.clone();
    if let Some(m) = s.as_object_mut() {
        m.entry("region").or_insert(json!("nyc"));
        if let Some(Value::Array(svcs)) = m.get_mut("services") {
            for v in svcs.iter_mut().filter_map(Value::as_object_mut) {
                v.entry("instance_count").or_insert(json!(1));
                v.entry("instance_size_slug").or_insert(json!("basic-xxs"));
            }
        }
    }
    s
}

/// A read: the newest deployment goes on, through its phases, to active (the one before
/// superseded), or to an error.
fn do_step(a: &mut DoApp) {
    if a.busy == 0 {
        return;
    }
    a.busy -= 1;
    let broken = a.deployments.first().is_some_and(|d| d.to_string().contains("broken"));
    let phase = match a.busy {
        2 => "BUILDING",
        1 => "DEPLOYING",
        _ if broken => "ERROR",
        _ => "ACTIVE",
    };
    if phase == "ACTIVE" {
        for d in a.deployments.iter_mut().skip(1).filter(|d| d.get("phase").and_then(Value::as_str) == Some("ACTIVE")) {
            set(d, "phase", json!("SUPERSEDED"));
        }
    }
    if let Some(d) = a.deployments.first_mut() {
        set(d, "phase", json!(phase));
    }
}

fn do_answer(id: &str, a: &DoApp) -> Value {
    let active = a.deployments.iter().find(|d| d.get("phase").and_then(Value::as_str) == Some("ACTIVE")).cloned();
    let moving = a.deployments.first().filter(|d| matches!(d.get("phase").and_then(Value::as_str), Some("PENDING_BUILD" | "BUILDING" | "DEPLOYING"))).cloned();
    let name = a.spec.get("name").and_then(Value::as_str).unwrap_or_default();
    json!({ "id": id, "spec": a.spec, "live_url": format!("https://{name}-abc.ondigitalocean.app"), "updated_at": "2026-10-23T00:00:00Z",
        "active_deployment": active, "in_progress_deployment": moving, "pending_deployment": moving })
}

fn digitalocean(st: &mut HostingState, method: &http::Method, rest: &[&str], body: &Value) -> Response {
    let n = next(st);
    match (method.as_str(), rest) {
        ("GET", []) => ok(json!({ "apps": st.apps.iter().map(|(id, a)| do_answer(id, a)).collect::<Vec<_>>() })),
        ("POST", []) => {
            let Some(spec) = body.get("spec").filter(|s| s.get("name").is_some()) else { return err(422, "unprocessable_entity", "the spec needs a name") };
            let id = format!("app-{n}");
            let spec = do_defaulted(spec);
            let d = json!({ "id": format!("dep-{n}"), "phase": "PENDING_BUILD", "cause": "app created", "spec": spec });
            let a = DoApp { spec, deployments: vec![d], busy: 3 };
            st.calls.push(format!("CreateApp {}", a.spec.get("name").and_then(Value::as_str).unwrap_or_default()));
            let answer = do_answer(&id, &a);
            st.apps.insert(id, a);
            ok(json!({ "app": answer }))
        }
        (_, [id, rest @ ..]) => {
            let id = (*id).to_owned();
            let Some(a) = st.apps.get_mut(&id) else { return err(404, "not_found", "The resource you were accessing could not be found.") };
            let name = a.spec.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
            match (method.as_str(), rest) {
                ("GET", []) => {
                    do_step(a);
                    ok(json!({ "app": do_answer(&id, a) }))
                }
                ("PUT", []) => {
                    let Some(spec) = body.get("spec") else { return err(422, "unprocessable_entity", "a spec") };
                    a.spec = do_defaulted(spec);
                    a.deployments.insert(0, json!({ "id": format!("dep-{n}"), "phase": "PENDING_BUILD", "cause": "app spec updated", "spec": a.spec }));
                    a.busy = 3;
                    let answer = do_answer(&id, a);
                    st.calls.push(format!("UpdateApp {name}"));
                    ok(json!({ "app": answer }))
                }
                ("DELETE", []) => {
                    st.apps.remove(&id);
                    st.calls.push(format!("DeleteApp {name}"));
                    ok(json!({ "id": id }))
                }
                ("GET", ["deployments"]) => {
                    do_step(a);
                    ok(json!({ "deployments": a.deployments }))
                }
                ("GET", ["deployments", did]) => {
                    do_step(a);
                    match a.deployments.iter().find(|d| d.get("id").and_then(Value::as_str) == Some(*did)) {
                        Some(d) => ok(json!({ "deployment": d })),
                        None => err(404, "not_found", "no such deployment"),
                    }
                }
                ("POST", ["rollback"]) => {
                    let target = body.get("deployment_id").and_then(Value::as_str).unwrap_or_default();
                    let Some(before) = a.deployments.iter().find(|d| d.get("id").and_then(Value::as_str) == Some(target)).cloned() else { return err(404, "not_found", "no such deployment") };
                    a.spec = before.get("spec").cloned().unwrap_or(Value::Null);
                    let d = json!({ "id": format!("dep-{n}"), "phase": "DEPLOYING", "cause": "rollback", "spec": a.spec });
                    a.deployments.insert(0, d.clone());
                    a.busy = 1;
                    st.calls.push(format!("RollbackApp {name} {target}"));
                    ok(json!({ "deployment": d }))
                }
                _ => err(404, "not_found", "the fake has no such App Platform call"),
            }
        }
        _ => err(404, "not_found", "the fake has no such App Platform call"),
    }
}

// ------------------------------------------------------------------ Heroku

/// A read: the newest build goes on, and once it's done its release is current; the dynos start
/// on the current release, or crash on a slug that does.
fn heroku_step(st: &mut HostingState, name: &str) {
    let n = next(st);
    let blobs = st.blobs.clone();
    let Some(a) = st.heroku.get_mut(name) else { return };
    if a.building > 0 {
        a.building -= 1;
        if a.building == 0 {
            let url = a.builds.first().and_then(|b| b.pointer("/source_blob/url")).and_then(Value::as_str).unwrap_or_default().to_owned();
            let id = url.rsplit('/').next().unwrap_or_default().split('?').next().unwrap_or_default().to_owned();
            let source = blobs.get(&id).map(|b| String::from_utf8_lossy(b).into_owned());
            match source {
                Some(src) if !src.contains("unbuildable") => {
                    let slug = format!("slug-{n}");
                    if src.contains("broken") {
                        a.crashing.push(slug.clone());
                    }
                    let release = heroku_release(a, n, &slug, "Deploy");
                    if let Some(b) = a.builds.first_mut() {
                        set(b, "status", json!("succeeded"));
                        set(b, "release", json!({ "id": release }));
                        set(b, "slug", json!({ "id": slug }));
                    }
                }
                _ => {
                    if let Some(b) = a.builds.first_mut() {
                        set(b, "status", json!("failed"));
                    }
                }
            }
        }
    } else if a.starting > 0 {
        a.starting -= 1;
    }
}

/// A new release of `slug`, current: its id.
fn heroku_release(a: &mut HerokuApp, n: u64, slug: &str, what: &str) -> String {
    for r in &mut a.releases {
        set(r, "current", json!(false));
    }
    let version = a.releases.len() + 1;
    let id = format!("rel-{n}");
    a.releases.insert(0, json!({ "id": id, "version": version, "status": "succeeded", "current": true, "slug": { "id": slug }, "description": format!("{what} v{version}"), "created_at": format!("2026-10-23T00:{:02}:00Z", n % 60) }));
    a.starting = 1;
    id
}

fn heroku_dynos(a: &HerokuApp) -> Value {
    let Some(r) = a.releases.iter().find(|r| r.get("current").and_then(Value::as_bool) == Some(true)) else { return json!([]) };
    let slug = r.pointer("/slug/id").and_then(Value::as_str).unwrap_or_default();
    let state = if a.starting > 0 { "starting" } else if a.crashing.iter().any(|c| c == slug) { "crashed" } else { "up" };
    json!([{ "type": "web", "name": "web.1", "state": state, "release": { "id": r.get("id"), "version": r.get("version") } }, { "type": "web", "name": "web.2", "state": state, "release": { "id": r.get("id"), "version": r.get("version") } }])
}

fn heroku(st: &mut HostingState, method: &http::Method, rest: &[&str], body: &Value) -> Response {
    if let ([], "GET") = (rest, method.as_str()) {
        return ok(Value::Array(st.heroku.keys().map(|n| json!({ "name": n, "web_url": format!("https://{n}-abc.herokuapp.com/") })).collect()));
    }
    if let ([], "POST") = (rest, method.as_str()) {
        let name = body.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
        if st.heroku.contains_key(&name) {
            return err(422, "invalid_params", "Name is already taken");
        }
        st.heroku.insert(name.clone(), HerokuApp::default());
        st.calls.push(format!("CreateApp {name}"));
        return srv::json(http::StatusCode::CREATED, &json!({ "name": name }));
    }
    let Some(name) = rest.first().map(|n| (*n).to_owned()) else { return err(404, "not_found", "no such call") };
    if !st.heroku.contains_key(&name) {
        return err(404, "not_found", "Couldn't find that app.");
    }
    let n = next(st);
    match (method.as_str(), rest.get(1..).unwrap_or_default()) {
        ("GET", []) => ok(json!({ "name": name, "web_url": format!("https://{name}-abc.herokuapp.com/"), "stack": { "name": "heroku-24" } })),
        ("DELETE", []) => {
            st.heroku.remove(&name);
            st.calls.push(format!("DeleteApp {name}"));
            ok(json!({ "name": name }))
        }
        ("GET", ["config-vars"]) => ok(Value::Object(st.heroku.get(&name).map(|a| a.config.clone()).unwrap_or_default())),
        ("PATCH", ["config-vars"]) => {
            let Some(a) = st.heroku.get_mut(&name) else { return err(404, "not_found", "no app") };
            for (k, v) in body.as_object().into_iter().flatten() {
                a.config.insert(k.clone(), v.clone());
            }
            // A config change is a release of the current slug.
            let slug = a.releases.first().and_then(|r| r.pointer("/slug/id")).and_then(Value::as_str).map(str::to_owned);
            if let Some(slug) = slug {
                heroku_release(a, n, &slug, "Set config vars");
            }
            st.calls.push(format!("SetConfig {name}"));
            ok(Value::Object(st.heroku.get(&name).map(|a| a.config.clone()).unwrap_or_default()))
        }
        ("GET", ["dynos"]) => {
            heroku_step(st, &name);
            ok(st.heroku.get(&name).map(heroku_dynos).unwrap_or_default())
        }
        ("GET", ["builds"]) => ok(Value::Array(st.heroku.get(&name).map(|a| a.builds.iter().take(1).cloned().collect()).unwrap_or_default())),
        ("POST", ["builds"]) => {
            let Some(a) = st.heroku.get_mut(&name) else { return err(404, "not_found", "no app") };
            let b = json!({ "id": format!("build-{n}"), "status": "pending", "source_blob": body.get("source_blob"), "created_at": format!("2026-10-23T00:{:02}:00Z", n % 60) });
            a.builds.insert(0, b.clone());
            a.building = 2;
            st.calls.push(format!("Build {name}"));
            srv::json(http::StatusCode::CREATED, &b)
        }
        ("GET", ["builds", id]) => {
            heroku_step(st, &name);
            match st.heroku.get(&name).and_then(|a| a.builds.iter().find(|b| b.get("id").and_then(Value::as_str) == Some(*id)).cloned()) {
                Some(b) => ok(b),
                None => err(404, "not_found", "no such build"),
            }
        }
        ("GET", ["releases"]) => ok(Value::Array(st.heroku.get(&name).map(|a| a.releases.clone()).unwrap_or_default())),
        ("GET", ["releases", id]) => match st.heroku.get(&name).and_then(|a| a.releases.iter().find(|r| r.get("id").and_then(Value::as_str) == Some(*id)).cloned()) {
            Some(r) => ok(r),
            None => err(404, "not_found", "no such release"),
        },
        ("POST", ["releases"]) => {
            let target = body.get("release").and_then(Value::as_str).unwrap_or_default().to_owned();
            let Some(a) = st.heroku.get_mut(&name) else { return err(404, "not_found", "no app") };
            let Some(before) = a.releases.iter().find(|r| r.get("id").and_then(Value::as_str) == Some(target.as_str())).cloned() else { return err(404, "not_found", "no such release") };
            let slug = before.pointer("/slug/id").and_then(Value::as_str).unwrap_or_default().to_owned();
            let id = heroku_release(a, n, &slug, &format!("Rollback to v{}", before.get("version").and_then(Value::as_u64).unwrap_or(0)));
            let r = a.releases.first().cloned().unwrap_or_default();
            st.calls.push(format!("Rollback {name} {target}"));
            let _ = id;
            srv::json(http::StatusCode::CREATED, &r)
        }
        _ => err(404, "not_found", "the fake has no such Heroku call"),
    }
}

impl HostingState {
    /// A service made on Render, outside kiln.
    pub fn render_service(&mut self, name: &str) {
        let id = format!("srv-{}", self.render.len() + 1);
        self.render.insert(name.to_owned(), RenderService { id, ..RenderService::default() });
    }
}

/// Starts the fake on a free port, and points kiln's hosting platforms at it: its base URL.
pub async fn start(fake: Arc<FakeHosting>) -> anyhow::Result<String> {
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
    for kind in ["render", "netlify", "heroku", "digitalocean", "fly"] {
        crate::ci::integrations::override_endpoint(kind, &base);
    }
    Ok(base)
}

/// The test account's target for `provider`, and its place as an environment names it.
pub fn target(provider: &str) -> anyhow::Result<(kiln_cd::providers::Target, Value)> {
    let place = json!({ "account": format!("{provider}-test") });
    let (t, _) = kiln_cd::providers::target_of(provider, place.as_object().ok_or_else(|| anyhow::anyhow!("place"))?).map_err(|e| anyhow::anyhow!("{e}"))?;
    anyhow::ensure!(t.def().is_some_and(|d| d.built), "{provider} isn't built");
    Ok((t, place))
}
