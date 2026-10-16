//! HTTP + WebSocket API for the UI and CLI (see docs/api.md), plus the small set of endpoints
//! agents use over HTTP: enrollment, source archives and artifact upload. Served by kiln's own
//! HTTP layer (hyper's protocol code on hyper-rt).
//!
//! Auth is stateless so it survives restarts: sessions and one-time login links are HMACs of an
//! expiry under the admin token; job tokens are HMACs of the job id.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt as _;
use http::{HeaderMap, Method, StatusCode, header};
use serde::Deserialize;
use serde_json::json;

use crate::agent::{EnrollRequest, EnrollResponse};
use crate::http::server::{self as srv, Request, Response};
use crate::http::ws::{self, Message};
use crate::hub::{Hub, TermOut};
use crate::logs::LogEvent;
use crate::proto::{LANE_TERM, ServerMsg};
use crate::store::Repo;

pub struct ApiError(StatusCode, String);

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

impl ApiError {
    fn into_response(self) -> Response {
        if self.0.is_server_error() {
            tracing::warn!(status = %self.0, error = %self.1, "api error");
        }
        srv::json(self.0, &json!({ "error": self.1 }))
    }
}

fn err(code: StatusCode, msg: impl Into<String>) -> ApiError {
    ApiError(code, msg.into())
}

type R = Result<Response, ApiError>;

const SESSION_COOKIE: &str = "kiln_session";
const SESSION_TTL: i64 = 30 * 86_400;
const OTP_TTL: i64 = 300;
const JSON_LIMIT: usize = 4 << 20;

// ------------------------------------------------------------------ auth

fn sign(hub: &Hub, purpose: &str, payload: &str) -> String {
    crate::util::hmac_hex(hub.admin_token.as_bytes(), format!("{purpose}:{payload}").as_bytes())
}

fn signed_expiring(hub: &Hub, purpose: &str, ttl: i64) -> String {
    let exp = crate::util::now_unix() + ttl;
    format!("{exp}.{}", sign(hub, purpose, &exp.to_string()))
}

fn verify_expiring(hub: &Hub, purpose: &str, value: &str) -> bool {
    let Some((exp, sig)) = value.split_once('.') else { return false };
    let Ok(e) = exp.parse::<i64>() else { return false };
    e > crate::util::now_unix() && crate::util::ct_eq(sig.as_bytes(), sign(hub, purpose, exp).as_bytes())
}

pub fn otp_url(hub_admin_token: &str, public_url: &str) -> String {
    let exp = crate::util::now_unix() + OTP_TTL;
    let sig = crate::util::hmac_hex(hub_admin_token.as_bytes(), format!("otp:{exp}").as_bytes());
    format!("{}/api/auth/otp?code={exp}.{sig}", public_url.trim_end_matches('/'))
}

pub fn job_token(admin_token: &str, job: &str) -> String {
    crate::util::hmac_hex(admin_token.as_bytes(), format!("job:{job}").as_bytes())
}

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_owned())
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ")
}

fn authed(hub: &Hub, headers: &HeaderMap) -> bool {
    if let Some(t) = bearer(headers)
        && crate::util::ct_eq(t.as_bytes(), hub.admin_token.as_bytes())
    {
        return true;
    }
    cookie(headers, SESSION_COOKIE).is_some_and(|c| verify_expiring(hub, "session", &c))
}

fn session_cookie(hub: &Hub) -> String {
    let secure = if hub.cfg.server.public_url.starts_with("https") { "; Secure" } else { "" };
    format!(
        "{SESSION_COOKIE}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={SESSION_TTL}{secure}",
        signed_expiring(hub, "session", SESSION_TTL)
    )
}

// ------------------------------------------------------------------ router

/// The API as an HTTP handler.
pub struct Api {
    pub hub: Arc<Hub>,
}

impl srv::Handler for Api {
    async fn handle(self: Arc<Self>, req: Request) -> Response {
        let hub = self.hub.clone();
        match route(hub, req).await {
            Ok(r) => r,
            Err(e) => e.into_response(),
        }
    }
}

fn segments(path: &str) -> Vec<String> {
    path.trim_matches('/').split('/').filter(|s| !s.is_empty()).map(srv::decode).collect()
}

async fn route(hub: Arc<Hub>, req: Request) -> R {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let seg = segments(&path);
    let s: Vec<&str> = seg.iter().map(String::as_str).collect();

    // Open routes (their own auth, if any).
    match (&method, s.as_slice()) {
        (&Method::POST, ["api", "auth", "login"]) => return login(&hub, req).await,
        (&Method::GET, ["api", "auth", "otp"]) => return Ok(otp(&hub, &req)),
        (&Method::GET, ["api", "auth", "methods"]) => return Ok(auth_methods(&hub).await),
        (&Method::POST, ["api", "auth", "password"]) => return login_password(&hub, req).await,
        (&Method::POST, ["api", "auth", "logout"]) => return Ok(logout()),
        (&Method::GET, ["api", "me"]) => return Ok(me(&hub, req.headers())),
        (&Method::POST, ["api", "agents", "enroll"]) => return enroll(&hub, req).await,
        (&Method::GET, ["api", "jobs", id, "source"]) => return job_source(&hub, id, req.headers()).await,
        (&Method::PUT, ["api", "jobs", id, "artifacts", "upload"]) => {
            let id = (*id).to_owned();
            return put_artifacts(&hub, &id, req).await;
        }
        (&Method::POST, ["api", "hooks", "github"]) => return github_hook(&hub, req).await,
        (&Method::GET, ["metrics"]) => return Ok(metrics_handler(&hub, req.headers())),
        (&Method::GET, ["healthz"]) => return Ok(srv::text(StatusCode::OK, "ok")),
        (&Method::GET, ["readyz"]) => return Ok(readyz(&hub).await),
        (&Method::GET, ["dl", "kiln"]) => return download_binary().await,
        (&Method::GET, ["install.sh"]) => return Ok(install_script(&hub)),
        _ => {}
    }

    let is_api = s.first() == Some(&"api");
    if !is_api {
        return Ok(crate::ui::serve(&req));
    }
    if !authed(&hub, req.headers()) {
        return Err(err(StatusCode::UNAUTHORIZED, "authentication required"));
    }
    let q = srv::query(&req);
    match (&method, s.as_slice()) {
        (&Method::GET, ["api", "overview"]) => overview(&hub).await,
        (&Method::GET, ["api", "system"]) => Ok(system(&hub).await),
        (&Method::GET, ["api", "audit"]) => audit(&hub, &q).await,
        (&Method::GET, ["api", "metrics", "history"]) => Ok(history(&hub, &q)),
        (&Method::GET, ["api", "repos"]) => Ok(srv::json(StatusCode::OK, &hub.store.repos().await?)),
        (&Method::POST, ["api", "repos"]) => add_repo(&hub, req).await,
        (&Method::DELETE, ["api", "repos", id]) => delete_repo(&hub, id).await,
        (&Method::POST, ["api", "repos", id, "poll"]) => poll_repo(&hub, id).await,
        (&Method::POST, ["api", "repos", id, "trigger"]) => {
            let id = (*id).to_owned();
            trigger(&hub, &id, req).await
        }
        (&Method::GET, ["api", "runs"]) => runs(&hub, &q).await,
        (&Method::GET, ["api", "runs", id]) => run(&hub, id).await,
        (&Method::POST, ["api", "runs", id, "cancel"]) => cancel(&hub, id).await,
        (&Method::POST, ["api", "runs", id, "retry"]) => {
            let id = (*id).to_owned();
            retry(&hub, &id, req).await
        }
        (&Method::GET, ["api", "jobs", id]) => job(&hub, id).await,
        (&Method::GET, ["api", "jobs", id, "logs"]) => job_logs(&hub, id, &q).await,
        (&Method::GET, ["api", "jobs", id, "logs", "ws"]) => {
            let (id, step) = ((*id).to_owned(), q.get("step").and_then(|v| v.parse().ok()).unwrap_or(0));
            job_logs_ws(hub.clone(), id, step, req)
        }
        (&Method::GET, ["api", "jobs", id, "artifacts"]) => get_artifacts(&hub, id).await,
        (&Method::GET, ["api", "agents"]) => Ok(srv::json(StatusCode::OK, &hub.agent_list().await)),
        (&Method::GET, ["api", "agents", "join"]) => Ok(join_info(&hub)),
        (&Method::DELETE, ["api", "agents", name]) => forget_agent(&hub, name).await,
        (&Method::POST, ["api", "agents", name, "drain"]) => {
            let name = (*name).to_owned();
            drain_agent(&hub, &name, req).await
        }
        (&Method::PUT, ["api", "settings", "password"]) => set_password(&hub, req).await,
        (&Method::DELETE, ["api", "settings", "password"]) => clear_password(&hub).await,
        (&Method::GET, ["api", "events"]) => events_ws(hub.clone(), req),
        (&Method::GET, ["api", "terminal", "ws"]) => terminal_ws(hub.clone(), q, req).await,
        _ => Err(err(StatusCode::NOT_FOUND, "not found")),
    }
}

// ------------------------------------------------------------------ auth handlers

#[derive(Deserialize)]
struct Login {
    token: String,
}

async fn login(hub: &Hub, req: Request) -> R {
    let l: Login = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    if crate::util::ct_eq(l.token.trim().as_bytes(), hub.admin_token.as_bytes()) {
        hub.store.audit("admin".into(), "login".into(), "token".into()).await;
        Ok(srv::with_header(srv::status(StatusCode::NO_CONTENT), header::SET_COOKIE, &session_cookie(hub)))
    } else {
        crate::rt::sleep(Duration::from_millis(500)).await;
        Err(err(StatusCode::UNAUTHORIZED, "invalid token"))
    }
}

fn otp(hub: &Hub, req: &Request) -> Response {
    let q = srv::query(req);
    let code = q.get("code").map(String::as_str).unwrap_or("");
    if verify_expiring(hub, "otp", code) {
        let hub_store = hub.store.clone();
        crate::rt::spawn(async move {
            hub_store.audit("admin".into(), "login".into(), "one-time link".into()).await;
        });
        let r = srv::with_header(srv::status(StatusCode::SEE_OTHER), header::LOCATION, "/");
        srv::with_header(r, header::SET_COOKIE, &session_cookie(hub))
    } else {
        // Back to the login page, which explains what went wrong and offers the other methods.
        srv::with_header(srv::status(StatusCode::SEE_OTHER), header::LOCATION, &format!("/?signin={}", otp_problem(hub, code)))
    }
}

/// Why a one-time link was refused: `expired` (ours, too old) or `foreign` (signed with another
/// server's admin token, e.g. `kiln login` run against a different installation).
fn otp_problem(hub: &Hub, code: &str) -> &'static str {
    let ours = code
        .split_once('.')
        .is_some_and(|(exp, sig)| crate::util::ct_eq(sig.as_bytes(), sign(hub, "otp", exp).as_bytes()));
    if ours { "expired" } else { "foreign" }
}

static PASSWORD_THROTTLE: std::sync::LazyLock<crate::password::Throttle> = std::sync::LazyLock::new(Default::default);
/// One password check at a time, so the throttle cannot be outrun by parallel guesses.
static PASSWORD_GATE: async_lock::Mutex<()> = async_lock::Mutex::new(());

/// Which sign-in methods the login page should offer (public).
async fn auth_methods(hub: &Hub) -> Response {
    let path = crate::password::path(&hub.cfg);
    let password = crate::rt::blocking(move || crate::password::load(&path).is_some()).await.unwrap_or(false);
    srv::json(StatusCode::OK, &json!({ "password": password, "token": true }))
}

#[derive(Deserialize)]
struct PasswordLogin {
    password: String,
}

async fn login_password(hub: &Hub, req: Request) -> R {
    let l: PasswordLogin =
        srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    let gate = PASSWORD_GATE.lock().await;
    if let Err(wait) = PASSWORD_THROTTLE.check() {
        let secs = wait.as_secs().max(1);
        let r = srv::json(
            StatusCode::TOO_MANY_REQUESTS,
            &json!({ "error": format!("Too many wrong passwords. Try again in {secs}s.") }),
        );
        return Ok(srv::with_header(r, header::RETRY_AFTER, &secs.to_string()));
    }
    let path = crate::password::path(&hub.cfg);
    let outcome = crate::rt::blocking(move || crate::password::load(&path).map(|phc| crate::password::verify(&l.password, &phc)))
        .await
        .unwrap_or(Some(false));
    match outcome {
        Some(true) => {
            PASSWORD_THROTTLE.succeeded();
            drop(gate);
            hub.store.audit("admin".into(), "login".into(), "password".into()).await;
            Ok(srv::with_header(srv::status(StatusCode::NO_CONTENT), header::SET_COOKIE, &session_cookie(hub)))
        }
        Some(false) => {
            PASSWORD_THROTTLE.failed();
            drop(gate);
            crate::rt::sleep(Duration::from_millis(300)).await;
            Err(err(StatusCode::UNAUTHORIZED, "Wrong password."))
        }
        None => Err(err(
            StatusCode::CONFLICT,
            "Password sign-in is not set up yet. Sign in with the admin token once, then choose a password in Settings.",
        )),
    }
}

#[derive(Deserialize)]
struct SetPassword {
    password: String,
}

async fn set_password(hub: &Hub, req: Request) -> R {
    let p: SetPassword =
        srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    crate::password::validate(&p.password).map_err(|why| err(StatusCode::BAD_REQUEST, format!("Password not accepted: {why}.")))?;
    let path = crate::password::path(&hub.cfg);
    crate::rt::blocking(move || crate::password::hash(&p.password).and_then(|h| crate::password::store(&path, &h))).await??;
    PASSWORD_THROTTLE.succeeded();
    hub.store.audit("admin".into(), "password".into(), "set".into()).await;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

async fn clear_password(hub: &Hub) -> R {
    let path = crate::password::path(&hub.cfg);
    crate::rt::blocking(move || crate::password::clear(&path)).await??;
    hub.store.audit("admin".into(), "password".into(), "cleared".into()).await;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

fn logout() -> Response {
    srv::with_header(srv::status(StatusCode::NO_CONTENT), header::SET_COOKIE, &format!("{SESSION_COOKIE}=; Path=/; Max-Age=0"))
}

fn me(hub: &Hub, headers: &HeaderMap) -> Response {
    srv::json(
        StatusCode::OK,
        &json!({
            "authenticated": authed(hub, headers),
            "public_url": hub.cfg.server.public_url,
            "version": env!("CARGO_PKG_VERSION"),
            "hostname": crate::config::Config::hostname(),
        }),
    )
}

// ------------------------------------------------------------------ overview / system

async fn overview(hub: &Hub) -> R {
    let stats = hub.store.stats().await?;
    let agents = hub.agent_list().await;
    let m = crate::metrics::global();
    let sample = hub.history.lock().get("server").and_then(|q| q.back().cloned());
    Ok(srv::json(
        StatusCode::OK,
        &json!({
            "stats": stats,
            "queue": { "queued": m.jobs_queued.get(), "running": m.jobs_running.get() },
            "agents": {
                "connected": agents.iter().filter(|a| a.connected).count(),
                "total": agents.len(),
                "slots": agents.iter().filter(|a| a.connected).map(|a| a.slots).sum::<u32>(),
                "busy": agents.iter().filter(|a| a.connected).map(|a| a.busy).sum::<u32>(),
            },
            "system": system_json(hub),
            "sample": sample,
        }),
    ))
}

fn system_json(hub: &Hub) -> serde_json::Value {
    let sample = hub.history.lock().get("server").and_then(|q| q.back().cloned());
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_s": hub.started.elapsed().as_secs(),
        "hostname": crate::config::Config::hostname(),
        "generation": hub.generation,
        "children": crate::server::registry().snapshot(),
        "sample": sample,
        "kubernetes": hub.k8s.get().is_some_and(Option::is_some),
        "runtime": "hyper-rt",
    })
}

async fn system(hub: &Hub) -> Response {
    let path = hub.cfg.data("crash.log");
    let crash = crate::rt::blocking(move || std::fs::read_to_string(path).unwrap_or_default()).await.unwrap_or_default();
    let lines: Vec<&str> = crash.lines().rev().take(200).collect();
    let mut cfg = serde_json::to_value(&*hub.cfg).unwrap_or_default();
    if let Some(s) = cfg.get_mut("secrets").and_then(|s| s.as_object_mut()) {
        for v in s.values_mut() {
            *v = json!("***");
        }
    }
    let mut v = system_json(hub);
    if let Some(o) = v.as_object_mut() {
        o.insert("crash_log".into(), json!(lines));
        o.insert("config".into(), cfg);
    }
    srv::json(StatusCode::OK, &v)
}

async fn audit(hub: &Hub, q: &HashMap<String, String>) -> R {
    let limit = q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(200);
    Ok(srv::json(StatusCode::OK, &hub.store.audit_log(limit).await?))
}

fn history(hub: &Hub, q: &HashMap<String, String>) -> Response {
    let h = hub.history.lock();
    let mut out = serde_json::Map::new();
    for (k, v) in h.iter() {
        if q.get("agent").is_none_or(|a| a == k) {
            out.insert(k.clone(), json!(v));
        }
    }
    srv::json(StatusCode::OK, &serde_json::Value::Object(out))
}

// ------------------------------------------------------------------ repos

#[derive(Deserialize)]
struct NewRepo {
    name: String,
    clone_url: Option<String>,
    #[serde(default)]
    branches: Vec<String>,
    poll_seconds: Option<u32>,
}

async fn add_repo(hub: &Hub, req: Request) -> R {
    let r: NewRepo = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    let name = r.name.trim().trim_start_matches("https://github.com/").trim_end_matches(".git").to_owned();
    if name.split('/').count() != 2 || name.contains(char::is_whitespace) {
        return Err(err(StatusCode::BAD_REQUEST, "name must be owner/repo"));
    }
    let repo = hub
        .store
        .upsert_repo(Repo {
            id: crate::util::new_id(),
            clone_url: r.clone_url.unwrap_or_else(|| format!("https://github.com/{name}.git")),
            name: name.clone(),
            branches: r.branches,
            poll_seconds: r.poll_seconds.unwrap_or(hub.cfg.gitops.poll_seconds),
            enabled: true,
            created_at: crate::util::now_unix(),
            last_polled_at: None,
            last_error: None,
        })
        .await?;
    hub.store.audit("admin".into(), "repo.add".into(), name).await;
    hub.gitops_kick.notify();
    Ok(srv::json(StatusCode::OK, &repo))
}

async fn find_repo(hub: &Hub, id: &str) -> Result<Repo, ApiError> {
    hub.store
        .repos()
        .await?
        .into_iter()
        .find(|r| r.id == id || r.name == id)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "no such repository"))
}

async fn delete_repo(hub: &Hub, id: &str) -> R {
    let repo = find_repo(hub, id).await?;
    hub.store.delete_repo(repo.id).await?;
    hub.store.audit("admin".into(), "repo.delete".into(), repo.name).await;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

async fn poll_repo(hub: &Hub, id: &str) -> R {
    let repo = find_repo(hub, id).await?;
    let n = crate::gitops::poll_repo(hub, &repo).await;
    let e = n.as_ref().err().map(|e| format!("{e:#}"));
    hub.store.repo_polled(repo.id, e.clone()).await?;
    Ok(srv::json(StatusCode::OK, &json!({ "triggered": n.unwrap_or(0), "error": e })))
}

#[derive(Deserialize, Default)]
struct TriggerReq {
    branch: Option<String>,
    pipeline: Option<String>,
}

async fn trigger(hub: &Hub, id: &str, req: Request) -> R {
    let repo = find_repo(hub, id).await?;
    let t: TriggerReq = srv::body_json(req.into_body(), JSON_LIMIT).await.unwrap_or_default();
    let runs = crate::gitops::manual(hub, &repo, t.branch.clone(), t.pipeline)
        .await
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    hub.store.audit("admin".into(), "run.trigger".into(), format!("{} {:?}", repo.name, t.branch)).await;
    Ok(srv::json(StatusCode::OK, &runs))
}

// ------------------------------------------------------------------ runs & jobs

async fn runs(hub: &Hub, q: &HashMap<String, String>) -> R {
    let limit = q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(50);
    let before = q.get("before").and_then(|v| v.parse().ok());
    Ok(srv::json(StatusCode::OK, &hub.store.runs(q.get("repo").cloned(), limit, before).await?))
}

async fn run(hub: &Hub, id: &str) -> R {
    let run = hub.store.run(id.to_owned()).await?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such run"))?;
    let jobs = hub.store.jobs(id.to_owned(), false).await?;
    Ok(srv::json(StatusCode::OK, &json!({ "run": run, "jobs": jobs })))
}

async fn cancel(hub: &Hub, id: &str) -> R {
    hub.cancel_run(id, "cancelled by user").await?;
    hub.store.audit("admin".into(), "run.cancel".into(), id.to_owned()).await;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

#[derive(Deserialize)]
struct RetryReq {
    #[serde(default = "yes")]
    only_failed: bool,
}

fn yes() -> bool {
    true
}

async fn retry(hub: &Hub, id: &str, req: Request) -> R {
    let only_failed = srv::body_json::<RetryReq>(req.into_body(), JSON_LIMIT).await.map(|r| r.only_failed).unwrap_or(true);
    let n = hub.store.retry_run(id.to_owned(), only_failed).await?;
    hub.emit_run(id).await;
    hub.wake.notify();
    hub.store.audit("admin".into(), "run.retry".into(), id.to_owned()).await;
    Ok(srv::json(StatusCode::OK, &json!({ "jobs": n })))
}

async fn job(hub: &Hub, id: &str) -> R {
    let j = hub.store.job(id.to_owned()).await?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such job"))?;
    Ok(srv::json(StatusCode::OK, &j))
}

async fn job_logs(hub: &Hub, id: &str, q: &HashMap<String, String>) -> R {
    let step = q.get("step").and_then(|v| v.parse().ok()).unwrap_or(0);
    let data = hub.logs.read(id, step).await;
    Ok(srv::response(StatusCode::OK, "text/plain; charset=utf-8", srv::full(data)))
}

fn job_logs_ws(hub: Arc<Hub>, job: String, step: u32, req: Request) -> R {
    let (resp, upgrade) = ws::accept(req).map_err(|r| ApiError(r.status, r.reason.into()))?;
    crate::rt::spawn(async move {
        match upgrade.await {
            Ok(socket) => {
                if let Err(e) = stream_logs(hub, job, step, socket).await {
                    tracing::debug!(error = %e, "log stream ended");
                }
            }
            Err(e) => tracing::debug!(error = %e, "log stream upgrade failed"),
        }
    });
    Ok(resp)
}

async fn stream_logs(hub: Arc<Hub>, job: String, step: u32, socket: ws::WebSocket) -> anyhow::Result<()> {
    use futures::FutureExt as _;
    let (mut tx, mut rx_ws) = socket.split();
    let live = hub.logs.is_live(&job);
    let mut rx = live.then(|| hub.logs.subscribe(&job));
    let backlog = hub.logs.read(&job, step).await;
    for chunk in backlog.chunks(64 * 1024) {
        tx.send(ws::binary(Bytes::copy_from_slice(chunk))).await?;
    }
    let Some(rx) = rx.as_mut() else {
        tx.send(ws::text(json!({"type": "end"}).to_string())).await?;
        tx.close(None).await.ok();
        return Ok(());
    };
    loop {
        futures::select_biased! {
            ev = rx.recv().fuse() => match ev {
                Ok(LogEvent::Data { idx, data }) if idx == step => tx.send(ws::binary(data)).await?,
                Ok(LogEvent::Data { .. }) => {}
                Ok(LogEvent::Step { idx, status }) => {
                    tx.send(ws::text(json!({"type": "step", "idx": idx, "status": status}).to_string())).await?;
                }
                Ok(LogEvent::End) | Err(async_broadcast::RecvError::Closed) => {
                    tx.send(ws::text(json!({"type": "end"}).to_string())).await?;
                    tx.close(None).await.ok();
                    return Ok(());
                }
                Err(async_broadcast::RecvError::Overflowed(n)) => {
                    let note = format!("\r\n\x1b[33m[kiln] viewer fell behind; {n} chunks skipped (reload for the full log)\x1b[0m\r\n");
                    tx.send(ws::binary(note.into_bytes())).await?;
                }
            },
            msg = rx_ws.next().fuse() => match msg {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return Ok(()),
                _ => {}
            },
        }
    }
}

fn job_token_ok(hub: &Hub, headers: &HeaderMap, job: &str) -> bool {
    bearer(headers).is_some_and(|t| crate::util::ct_eq(t.as_bytes(), job_token(&hub.admin_token, job).as_bytes()))
        || authed(hub, headers)
}

async fn job_source(hub: &Hub, id: &str, headers: &HeaderMap) -> R {
    if !job_token_ok(hub, headers, id) {
        return Err(err(StatusCode::UNAUTHORIZED, "bad job token"));
    }
    let job = hub.store.job(id.to_owned()).await?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such job"))?;
    let run = hub.store.run(job.run_id).await?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such run"))?;
    let repo = find_repo(hub, &run.repo_id).await?;
    let stdout = crate::gitops::archive(hub, &repo, &run.sha).await?;
    Ok(srv::response(StatusCode::OK, "application/x-tar", srv::channel_body(stdout)))
}

fn artifact_path(hub: &Hub, job: &str) -> std::path::PathBuf {
    let safe: String = job.chars().filter(char::is_ascii_alphanumeric).collect();
    hub.cfg.data("artifacts").join(format!("{safe}.tar"))
}

async fn put_artifacts(hub: &Hub, id: &str, req: Request) -> R {
    if !job_token_ok(hub, req.headers(), id) {
        return Err(err(StatusCode::UNAUTHORIZED, "bad job token"));
    }
    let path = artifact_path(hub, id);
    let (sink, done) = crate::rt::file_sink(path, 8 << 30)?;
    let mut body = req.into_body();
    while let Some(chunk) = srv::next_chunk(&mut body).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))? {
        if sink.send(chunk).await.is_err() {
            break; // the writer failed; its error is reported below
        }
    }
    drop(sink);
    done.await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

async fn get_artifacts(hub: &Hub, id: &str) -> R {
    let path = artifact_path(hub, id);
    let body = crate::rt::file_source(path).map_err(|_| err(StatusCode::NOT_FOUND, "no artifacts"))?;
    let r = srv::response(StatusCode::OK, "application/x-tar", srv::channel_body(body));
    Ok(srv::with_header(r, header::CONTENT_DISPOSITION, &format!("attachment; filename=\"artifacts-{id}.tar\"")))
}

// ------------------------------------------------------------------ agents

async fn forget_agent(hub: &Hub, name: &str) -> R {
    hub.forget_agent(name).await?;
    hub.store.audit("admin".into(), "agent.forget".into(), name.to_owned()).await;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

#[derive(Deserialize)]
struct DrainReq {
    draining: bool,
}

async fn drain_agent(hub: &Hub, name: &str, req: Request) -> R {
    let d: DrainReq = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    if hub.set_draining(name, d.draining).await {
        Ok(srv::status(StatusCode::NO_CONTENT))
    } else {
        Err(err(StatusCode::NOT_FOUND, "agent not connected"))
    }
}

fn join_info(hub: &Hub) -> Response {
    let url = hub.cfg.server.public_url.trim_end_matches('/').to_owned();
    let token = hub.join_token.clone();
    srv::json(
        StatusCode::OK,
        &json!({
            "token": token,
            "server": url,
            "quic": hub.cfg.server.quic_advertise,
            "command": format!("curl -fsSL {url}/install.sh | sh -s -- --token {token} --labels baremetal,bench"),
            "manual": format!("kiln agent --server {url} --token {token} --labels baremetal,bench"),
        }),
    )
}

async fn enroll(hub: &Hub, req: Request) -> R {
    let r: EnrollRequest = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    let persistent = crate::util::ct_eq(r.token.as_bytes(), hub.join_token.as_bytes());
    let ephemeral = !persistent && hub.ephemeral_tokens.lock().remove(&r.token).is_some();
    if !persistent && !ephemeral {
        crate::rt::sleep(Duration::from_millis(500)).await;
        return Err(err(StatusCode::UNAUTHORIZED, "invalid join token"));
    }
    let name = crate::pki::sanitize_name(&r.name);
    let days = if ephemeral { crate::pki::EPHEMERAL_DAYS } else { 365 };
    let cert = hub.pki.sign_csr(&r.csr, &name, days).map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let quic = if ephemeral {
        hub.cfg.kubernetes.server_quic.clone().unwrap_or_else(|| hub.cfg.server.quic_advertise.clone())
    } else {
        hub.cfg.server.quic_advertise.clone()
    };
    if persistent {
        hub.store.audit(name.clone(), "agent.enroll".into(), String::new()).await;
        tracing::info!(agent = %name, "agent enrolled");
    }
    Ok(srv::json(StatusCode::OK, &EnrollResponse { cert_pem: cert, ca_pem: hub.pki.ca_pem.clone(), quic, name }))
}

/// The running binary, so new agents can be installed straight from the server.
async fn download_binary() -> R {
    let exe = std::env::current_exe().map_err(anyhow::Error::from)?;
    let body = crate::rt::file_source(exe)?;
    Ok(srv::response(StatusCode::OK, "application/octet-stream", srv::channel_body(body)))
}

fn install_script(hub: &Hub) -> Response {
    let url = hub.cfg.server.public_url.trim_end_matches('/').to_owned();
    let script = include_str!("../assets/install-agent.sh").replace("@@KILN_SERVER@@", &url);
    srv::response(StatusCode::OK, "text/x-shellscript", srv::full(script))
}

// ------------------------------------------------------------------ webhooks

async fn github_hook(hub: &Hub, req: Request) -> R {
    let sig = srv::header_str(&req, &header::HeaderName::from_static("x-hub-signature-256")).unwrap_or("").to_owned();
    let body = srv::body_bytes(req.into_body(), 25 << 20).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    // A configured but absent secret file means "no secret" (e.g. an optional mounted Secret).
    let secret = match &hub.cfg.gitops.webhook_secret_file {
        Some(path) => {
            let p = path.clone();
            crate::rt::blocking(move || std::fs::read_to_string(p).ok()).await?.filter(|s| !s.trim().is_empty())
        }
        None => None,
    };
    if let Some(secret) = secret {
        let want = format!("sha256={}", crate::util::hmac_hex(secret.trim().as_bytes(), &body));
        if !crate::util::ct_eq(want.as_bytes(), sig.as_bytes()) {
            return Err(err(StatusCode::UNAUTHORIZED, "bad signature"));
        }
    }
    // A webhook only hints; the poll decides what changed (GitOps: state comes from git).
    hub.gitops_kick.notify();
    Ok(srv::status(StatusCode::ACCEPTED))
}

// ------------------------------------------------------------------ events ws

fn events_ws(hub: Arc<Hub>, req: Request) -> R {
    let (resp, upgrade) = ws::accept(req).map_err(|r| ApiError(r.status, r.reason.into()))?;
    crate::rt::spawn(async move {
        let Ok(socket) = upgrade.await else { return };
        let m = crate::metrics::global();
        m.ws_clients.add(1);
        if let Err(e) = pump_events(&hub, socket).await {
            tracing::debug!(error = %e, "events stream ended");
        }
        m.ws_clients.add(-1);
    });
    Ok(resp)
}

async fn pump_events(hub: &Hub, socket: ws::WebSocket) -> anyhow::Result<()> {
    use futures::FutureExt as _;
    let (mut tx, mut rx_ws) = socket.split();
    let mut rx = hub.events.subscribe();
    hub.emit_queue();
    loop {
        futures::select_biased! {
            ev = rx.recv().fuse() => match ev {
                Ok(text) => tx.send(ws::text(text.as_ref())).await?,
                Err(async_broadcast::RecvError::Overflowed(_)) => {
                    tx.send(ws::text(json!({"type": "resync"}).to_string())).await?;
                }
                Err(async_broadcast::RecvError::Closed) => return Ok(()),
            },
            msg = rx_ws.next().fuse() => match msg {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return Ok(()),
                _ => {}
            },
        }
    }
}

// ------------------------------------------------------------------ terminal ws

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TermCtl {
    Resize { cols: u16, rows: u16 },
    Input { data: String },
    Ping,
}

async fn terminal_ws(hub: Arc<Hub>, q: HashMap<String, String>, req: Request) -> R {
    if !hub.cfg.terminal.enabled {
        return Err(err(StatusCode::FORBIDDEN, "terminal disabled in config"));
    }
    if crate::metrics::global().terminal_sessions.get() >= i64::try_from(hub.cfg.terminal.max_sessions).unwrap_or(32) {
        return Err(err(StatusCode::TOO_MANY_REQUESTS, "too many terminal sessions"));
    }
    let (resp, upgrade) = ws::accept(req).map_err(|r| ApiError(r.status, r.reason.into()))?;
    let target = q.get("target").cloned().unwrap_or_else(|| "local".into());
    let cols = q.get("cols").and_then(|v| v.parse().ok()).unwrap_or(120);
    let rows = q.get("rows").and_then(|v| v.parse().ok()).unwrap_or(40);
    let cmd = q.get("cmd").cloned();
    crate::rt::spawn(async move {
        let Ok(socket) = upgrade.await else { return };
        hub.store.audit("admin".into(), "terminal.open".into(), target.clone()).await;
        let r = match target.strip_prefix("agent:") {
            Some(agent) => agent_terminal(hub.clone(), agent.to_owned(), cols, rows, cmd, socket).await,
            None => local_terminal(&hub, cols, rows, cmd, socket).await,
        };
        if let Err(e) = r {
            tracing::debug!(error = %e, "terminal session ended");
        }
    });
    Ok(resp)
}

async fn local_terminal(hub: &Hub, cols: u16, rows: u16, cmd: Option<String>, socket: ws::WebSocket) -> anyhow::Result<()> {
    use futures::FutureExt as _;
    let (mut tx, mut rx) = socket.split();
    let (pty, out) = match crate::terminal::spawn(hub.cfg.terminal.shell.as_deref(), cmd.as_deref(), cols, rows) {
        Ok(p) => p,
        Err(e) => {
            tx.send(ws::text(json!({"type": "error", "message": format!("{e:#}")}).to_string())).await?;
            return Ok(());
        }
    };
    loop {
        futures::select_biased! {
            o = out.recv().fuse() => match o {
                Ok(crate::terminal::PtyOut::Data(d)) => tx.send(ws::binary(d)).await?,
                Ok(crate::terminal::PtyOut::Exit(code)) => {
                    tx.send(ws::text(json!({"type": "exit", "code": code}).to_string())).await.ok();
                    return Ok(());
                }
                Err(_) => {
                    tx.send(ws::text(json!({"type": "exit", "code": null}).to_string())).await.ok();
                    return Ok(());
                }
            },
            m = rx.next().fuse() => match m {
                Some(Ok(Message::Binary(b))) => pty.write(b.to_vec()),
                Some(Ok(Message::Text(t))) => match serde_json::from_str::<TermCtl>(t.as_str()) {
                    Ok(TermCtl::Resize { cols, rows }) => pty.resize(cols, rows),
                    Ok(TermCtl::Input { data }) => pty.write(data.into_bytes()),
                    Ok(TermCtl::Ping) | Err(_) => {}
                },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return Ok(()),
                _ => {}
            },
        }
    }
}

async fn agent_terminal(
    hub: Arc<Hub>,
    agent: String,
    cols: u16,
    rows: u16,
    cmd: Option<String>,
    socket: ws::WebSocket,
) -> anyhow::Result<()> {
    use futures::FutureExt as _;
    let (mut tx, mut rx) = socket.split();
    let session = crate::util::new_id();
    let key = format!("{agent}/{session}");
    let (out_tx, out_rx) = async_channel::bounded::<TermOut>(512);
    hub.terms.lock().insert(key.clone(), out_tx);
    let opened = hub
        .send_to_agent(&agent, LANE_TERM, &ServerMsg::TermOpen { session: session.clone(), cols, rows, command: cmd })
        .await;
    if !opened {
        hub.terms.lock().remove(&key);
        tx.send(ws::text(json!({"type": "error", "message": format!("agent {agent} is not connected")}).to_string())).await?;
        return Ok(());
    }
    crate::metrics::global().terminal_sessions.add(1);
    let result: anyhow::Result<()> = async {
        loop {
            futures::select_biased! {
                o = out_rx.recv().fuse() => match o {
                    Ok(TermOut::Data(d)) => tx.send(ws::binary(d)).await?,
                    Ok(TermOut::Exit { code, error }) => {
                        let msg = match error {
                            Some(e) => json!({"type": "error", "message": e}),
                            None => json!({"type": "exit", "code": code}),
                        };
                        tx.send(ws::text(msg.to_string())).await.ok();
                        return Ok(());
                    }
                    Err(_) => return Ok(()),
                },
                m = rx.next().fuse() => match m {
                    Some(Ok(Message::Binary(b))) => {
                        hub.send_to_agent(&agent, LANE_TERM, &ServerMsg::TermInput { session: session.clone(), data: b.to_vec() }).await;
                    }
                    Some(Ok(Message::Text(t))) => match serde_json::from_str::<TermCtl>(t.as_str()) {
                        Ok(TermCtl::Resize { cols, rows }) => {
                            hub.send_to_agent(&agent, LANE_TERM, &ServerMsg::TermResize { session: session.clone(), cols, rows }).await;
                        }
                        Ok(TermCtl::Input { data }) => {
                            hub.send_to_agent(&agent, LANE_TERM, &ServerMsg::TermInput { session: session.clone(), data: data.into_bytes() }).await;
                        }
                        Ok(TermCtl::Ping) | Err(_) => {}
                    },
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return Ok(()),
                    _ => {}
                },
            }
        }
    }
    .await;
    hub.terms.lock().remove(&key);
    hub.send_to_agent(&agent, LANE_TERM, &ServerMsg::TermClose { session }).await;
    crate::metrics::global().terminal_sessions.add(-1);
    result
}

// ------------------------------------------------------------------ misc

fn metrics_handler(hub: &Hub, headers: &HeaderMap) -> Response {
    if !hub.cfg.server.metrics_public && !authed(hub, headers) {
        return err(StatusCode::UNAUTHORIZED, "authentication required").into_response();
    }
    srv::response(StatusCode::OK, "text/plain; version=0.0.4", srv::full(crate::metrics::render()))
}

async fn readyz(hub: &Hub) -> Response {
    match hub.store.call(|c| Ok(c.query_row("SELECT 1", [], |r| r.get::<_, i64>(0))?)).await {
        Ok(_) => srv::text(StatusCode::OK, "ready"),
        Err(e) => srv::text(StatusCode::SERVICE_UNAVAILABLE, &format!("store: {e:#}")),
    }
}
