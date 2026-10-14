//! HTTP + WebSocket API for the UI and CLI (see docs/api.md), plus the small set of endpoints
//! agents use over HTTP: enrollment, source archives and artifact upload.
//!
//! Auth is stateless so it survives restarts: sessions and one-time login links are HMACs of an
//! expiry under the admin token; job tokens are HMACs of the job id.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use futures::{SinkExt as _, StreamExt as _};
use serde::Deserialize;
use serde_json::json;
use tokio::io::AsyncWriteExt as _;

use crate::agent::{EnrollRequest, EnrollResponse};
use crate::hub::{Hub, TermOut};
use crate::logs::LogEvent;
use crate::proto::{LANE_TERM, ServerMsg};
use crate::store::Repo;

type S = State<Arc<Hub>>;

pub struct ApiError(StatusCode, String);

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.0.is_server_error() {
            tracing::warn!(status = %self.0, error = %self.1, "api error");
        }
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

fn err(code: StatusCode, msg: impl Into<String>) -> ApiError {
    ApiError(code, msg.into())
}

type R<T> = Result<T, ApiError>;

const SESSION_COOKIE: &str = "kiln_session";
const SESSION_TTL: i64 = 30 * 86_400;
const OTP_TTL: i64 = 300;

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
        && crate::util::ct_eq(t.as_bytes(), hub.admin_token.as_bytes()) {
            return true;
        }
    cookie(headers, SESSION_COOKIE).is_some_and(|c| verify_expiring(hub, "session", &c))
}

async fn require_auth(State(hub): S, req: Request, next: Next) -> Response {
    if authed(&hub, req.headers()) {
        next.run(req).await
    } else {
        err(StatusCode::UNAUTHORIZED, "authentication required").into_response()
    }
}

fn session_cookie(hub: &Hub) -> String {
    let secure = if hub.cfg.server.public_url.starts_with("https") { "; Secure" } else { "" };
    format!(
        "{SESSION_COOKIE}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={SESSION_TTL}{secure}",
        signed_expiring(hub, "session", SESSION_TTL)
    )
}

#[derive(Deserialize)]
struct Login {
    token: String,
}

async fn login(State(hub): S, Json(l): Json<Login>) -> Response {
    if crate::util::ct_eq(l.token.trim().as_bytes(), hub.admin_token.as_bytes()) {
        hub.store.audit("admin".into(), "login".into(), "token".into()).await;
        (StatusCode::NO_CONTENT, [(header::SET_COOKIE, session_cookie(&hub))]).into_response()
    } else {
        tokio::time::sleep(Duration::from_millis(500)).await;
        err(StatusCode::UNAUTHORIZED, "invalid token").into_response()
    }
}

#[derive(Deserialize)]
struct Otp {
    code: String,
}

async fn otp(State(hub): S, Query(q): Query<Otp>) -> Response {
    if verify_expiring(&hub, "otp", &q.code) {
        hub.store.audit("admin".into(), "login".into(), "one-time link".into()).await;
        ([(header::SET_COOKIE, session_cookie(&hub))], Redirect::to("/")).into_response()
    } else {
        (StatusCode::UNAUTHORIZED, "login link expired; run `kiln login` again").into_response()
    }
}

async fn logout() -> Response {
    (StatusCode::NO_CONTENT, [(header::SET_COOKIE, format!("{SESSION_COOKIE}=; Path=/; Max-Age=0"))]).into_response()
}

async fn me(State(hub): S, headers: HeaderMap) -> Json<serde_json::Value> {
    Json(json!({
        "authenticated": authed(&hub, &headers),
        "version": env!("CARGO_PKG_VERSION"),
        "hostname": crate::config::Config::hostname(),
    }))
}

// ------------------------------------------------------------------ overview / system

async fn overview(State(hub): S) -> R<Json<serde_json::Value>> {
    let stats = hub.store.stats().await?;
    let agents = hub.agent_list().await;
    let m = crate::metrics::global();
    let sample = hub.history.lock().get("server").and_then(|q| q.back().cloned());
    Ok(Json(json!({
        "stats": stats,
        "queue": { "queued": m.jobs_queued.get(), "running": m.jobs_running.get() },
        "agents": {
            "connected": agents.iter().filter(|a| a.connected).count(),
            "total": agents.len(),
            "slots": agents.iter().filter(|a| a.connected).map(|a| a.slots).sum::<u32>(),
            "busy": agents.iter().filter(|a| a.connected).map(|a| a.busy).sum::<u32>(),
        },
        "system": system_json(&hub),
        "sample": sample,
    })))
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
    })
}

async fn system(State(hub): S) -> Json<serde_json::Value> {
    let crash = tokio::fs::read_to_string(hub.cfg.data("crash.log")).await.unwrap_or_default();
    let lines: Vec<&str> = crash.lines().rev().take(200).collect();
    let mut cfg = serde_json::to_value(&*hub.cfg).unwrap_or_default();
    if let Some(s) = cfg.get_mut("secrets").and_then(|s| s.as_object_mut()) {
        for v in s.values_mut() {
            *v = json!("***");
        }
    }
    let mut v = system_json(&hub);
    if let Some(o) = v.as_object_mut() {
        o.insert("crash_log".into(), json!(lines));
        o.insert("config".into(), cfg);
    }
    Json(v)
}

#[derive(Deserialize)]
struct Limit {
    limit: Option<u32>,
}

async fn audit(State(hub): S, Query(q): Query<Limit>) -> R<Json<Vec<serde_json::Value>>> {
    Ok(Json(hub.store.audit_log(q.limit.unwrap_or(200)).await?))
}

#[derive(Deserialize)]
struct HistoryQ {
    agent: Option<String>,
}

async fn history(State(hub): S, Query(q): Query<HistoryQ>) -> Json<serde_json::Value> {
    let h = hub.history.lock();
    let mut out = serde_json::Map::new();
    for (k, v) in h.iter() {
        if q.agent.as_ref().is_none_or(|a| a == k) {
            out.insert(k.clone(), json!(v));
        }
    }
    Json(serde_json::Value::Object(out))
}

// ------------------------------------------------------------------ repos

async fn repos(State(hub): S) -> R<Json<Vec<Repo>>> {
    Ok(Json(hub.store.repos().await?))
}

#[derive(Deserialize)]
struct NewRepo {
    name: String,
    clone_url: Option<String>,
    #[serde(default)]
    branches: Vec<String>,
    poll_seconds: Option<u32>,
}

async fn add_repo(State(hub): S, Json(r): Json<NewRepo>) -> R<Json<Repo>> {
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
    hub.gitops_kick.notify_one();
    Ok(Json(repo))
}

async fn find_repo(hub: &Hub, id: &str) -> R<Repo> {
    hub.store
        .repos()
        .await?
        .into_iter()
        .find(|r| r.id == id || r.name == id)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "no such repository"))
}

async fn delete_repo(State(hub): S, Path(id): Path<String>) -> R<StatusCode> {
    let repo = find_repo(&hub, &id).await?;
    hub.store.delete_repo(repo.id).await?;
    hub.store.audit("admin".into(), "repo.delete".into(), repo.name).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn poll_repo(State(hub): S, Path(id): Path<String>) -> R<Json<serde_json::Value>> {
    let repo = find_repo(&hub, &id).await?;
    let n = crate::gitops::poll_repo(&hub, &repo).await;
    let e = n.as_ref().err().map(|e| format!("{e:#}"));
    hub.store.repo_polled(repo.id, e.clone()).await?;
    Ok(Json(json!({ "triggered": n.unwrap_or(0), "error": e })))
}

#[derive(Deserialize, Default)]
struct TriggerReq {
    branch: Option<String>,
    pipeline: Option<String>,
}

async fn trigger(State(hub): S, Path(id): Path<String>, body: Option<Json<TriggerReq>>) -> R<Json<Vec<crate::store::Run>>> {
    let repo = find_repo(&hub, &id).await?;
    let t = body.map(|b| b.0).unwrap_or_default();
    let runs = crate::gitops::manual(&hub, &repo, t.branch.clone(), t.pipeline)
        .await
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    hub.store.audit("admin".into(), "run.trigger".into(), format!("{} {:?}", repo.name, t.branch)).await;
    Ok(Json(runs))
}

// ------------------------------------------------------------------ runs & jobs

#[derive(Deserialize)]
struct RunsQ {
    repo: Option<String>,
    limit: Option<u32>,
    before: Option<i64>,
}

async fn runs(State(hub): S, Query(q): Query<RunsQ>) -> R<Json<Vec<crate::store::Run>>> {
    Ok(Json(hub.store.runs(q.repo, q.limit.unwrap_or(50), q.before).await?))
}

async fn run(State(hub): S, Path(id): Path<String>) -> R<Json<serde_json::Value>> {
    let run = hub.store.run(id.clone()).await?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such run"))?;
    let jobs = hub.store.jobs(id, false).await?;
    Ok(Json(json!({ "run": run, "jobs": jobs })))
}

async fn cancel(State(hub): S, Path(id): Path<String>) -> R<StatusCode> {
    hub.cancel_run(&id, "cancelled by user").await?;
    hub.store.audit("admin".into(), "run.cancel".into(), id).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, Default)]
struct RetryReq {
    #[serde(default)]
    only_failed: bool,
}

async fn retry(State(hub): S, Path(id): Path<String>, body: Option<Json<RetryReq>>) -> R<Json<serde_json::Value>> {
    let only_failed = body.map(|b| b.0.only_failed).unwrap_or(true);
    let n = hub.store.retry_run(id.clone(), only_failed).await?;
    hub.emit_run(&id).await;
    hub.wake.notify_one();
    hub.store.audit("admin".into(), "run.retry".into(), id).await;
    Ok(Json(json!({ "jobs": n })))
}

async fn job(State(hub): S, Path(id): Path<String>) -> R<Json<crate::store::Job>> {
    Ok(Json(hub.store.job(id).await?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such job"))?))
}

#[derive(Deserialize)]
struct StepQ {
    step: Option<u32>,
}

async fn job_logs(State(hub): S, Path(id): Path<String>, Query(q): Query<StepQ>) -> Response {
    let data = hub.logs.read(&id, q.step.unwrap_or(0)).await;
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], data).into_response()
}

async fn job_logs_ws(State(hub): S, Path(id): Path<String>, Query(q): Query<StepQ>, ws: WebSocketUpgrade) -> Response {
    let step = q.step.unwrap_or(0);
    ws.on_upgrade(move |socket| async move {
        if let Err(e) = stream_logs(hub, id, step, socket).await {
            tracing::debug!(error = %e, "log stream ended");
        }
    })
}

async fn stream_logs(hub: Arc<Hub>, job: String, step: u32, mut socket: WebSocket) -> anyhow::Result<()> {
    let live = hub.logs.is_live(&job);
    let mut rx = live.then(|| hub.logs.subscribe(&job));
    let backlog = hub.logs.read(&job, step).await;
    for chunk in backlog.chunks(64 * 1024) {
        socket.send(Message::Binary(chunk.to_vec().into())).await?;
    }
    let Some(rx) = rx.as_mut() else {
        socket.send(Message::Text(json!({"type": "end"}).to_string().into())).await?;
        return Ok(());
    };
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(LogEvent::Data { idx, data }) if idx == step => socket.send(Message::Binary(data)).await?,
                Ok(LogEvent::Data { .. }) => {}
                Ok(LogEvent::Step { idx, status }) => {
                    socket.send(Message::Text(json!({"type": "step", "idx": idx, "status": status}).to_string().into())).await?;
                }
                Ok(LogEvent::End) | Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    socket.send(Message::Text(json!({"type": "end"}).to_string().into())).await?;
                    return Ok(());
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    let note = format!("\r\n\x1b[33m[kiln] viewer fell behind; {n} chunks skipped (reload for the full log)\x1b[0m\r\n");
                    socket.send(Message::Binary(note.into_bytes().into())).await?;
                }
            },
            msg = socket.recv() => match msg {
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

async fn job_source(State(hub): S, Path(id): Path<String>, headers: HeaderMap) -> R<Response> {
    if !job_token_ok(&hub, &headers, &id) {
        return Err(err(StatusCode::UNAUTHORIZED, "bad job token"));
    }
    let job = hub.store.job(id).await?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such job"))?;
    let run = hub.store.run(job.run_id).await?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such run"))?;
    let repo = find_repo(&hub, &run.repo_id).await?;
    let mut child = crate::gitops::archive(&hub, &repo, &run.sha).await?;
    let stdout = child.stdout.take().ok_or_else(|| err(StatusCode::INTERNAL_SERVER_ERROR, "no archive stream"))?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    let stream = tokio_util::io::ReaderStream::new(stdout);
    Ok(([(header::CONTENT_TYPE, "application/x-tar")], Body::from_stream(stream)).into_response())
}

fn artifact_path(hub: &Hub, job: &str) -> std::path::PathBuf {
    let safe: String = job.chars().filter(char::is_ascii_alphanumeric).collect();
    hub.cfg.data("artifacts").join(format!("{safe}.tar"))
}

async fn put_artifacts(State(hub): S, Path(id): Path<String>, headers: HeaderMap, body: Body) -> R<StatusCode> {
    if !job_token_ok(&hub, &headers, &id) {
        return Err(err(StatusCode::UNAUTHORIZED, "bad job token"));
    }
    let path = artifact_path(&hub, &id);
    if let Some(d) = path.parent() {
        tokio::fs::create_dir_all(d).await.map_err(anyhow::Error::from)?;
    }
    let tmp = path.with_extension("part");
    let mut f = tokio::fs::File::create(&tmp).await.map_err(anyhow::Error::from)?;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
        f.write_all(&chunk).await.map_err(anyhow::Error::from)?;
    }
    f.flush().await.map_err(anyhow::Error::from)?;
    tokio::fs::rename(&tmp, &path).await.map_err(anyhow::Error::from)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_artifacts(State(hub): S, Path(id): Path<String>) -> R<Response> {
    let path = artifact_path(&hub, &id);
    let f = tokio::fs::File::open(&path).await.map_err(|_| err(StatusCode::NOT_FOUND, "no artifacts"))?;
    let name = format!("attachment; filename=\"artifacts-{id}.tar\"");
    Ok((
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("application/x-tar")),
            (header::CONTENT_DISPOSITION, HeaderValue::from_str(&name).unwrap_or(HeaderValue::from_static("attachment"))),
        ],
        Body::from_stream(tokio_util::io::ReaderStream::new(f)),
    )
        .into_response())
}

// ------------------------------------------------------------------ agents

async fn agents(State(hub): S) -> Json<Vec<crate::hub::AgentLive>> {
    Json(hub.agent_list().await)
}

async fn forget_agent(State(hub): S, Path(name): Path<String>) -> R<StatusCode> {
    hub.forget_agent(&name).await?;
    hub.store.audit("admin".into(), "agent.forget".into(), name).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct DrainReq {
    draining: bool,
}

async fn drain_agent(State(hub): S, Path(name): Path<String>, Json(d): Json<DrainReq>) -> R<StatusCode> {
    if hub.set_draining(&name, d.draining).await {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(err(StatusCode::NOT_FOUND, "agent not connected"))
    }
}

async fn join_info(State(hub): S) -> Json<serde_json::Value> {
    let url = hub.cfg.server.public_url.trim_end_matches('/').to_owned();
    let token = hub.join_token.clone();
    Json(json!({
        "token": token,
        "server": url,
        "quic": hub.cfg.server.quic_advertise,
        "command": format!("curl -fsSL {url}/install.sh | sh -s -- --token {token} --labels baremetal,bench"),
        "manual": format!("kiln agent --server {url} --token {token} --labels baremetal,bench"),
    }))
}

async fn enroll(State(hub): S, Json(r): Json<EnrollRequest>) -> R<Json<EnrollResponse>> {
    let persistent = crate::util::ct_eq(r.token.as_bytes(), hub.join_token.as_bytes());
    let ephemeral = !persistent && hub.ephemeral_tokens.lock().remove(&r.token).is_some();
    if !persistent && !ephemeral {
        tokio::time::sleep(Duration::from_millis(500)).await;
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
    Ok(Json(EnrollResponse { cert_pem: cert, ca_pem: hub.pki.ca_pem.clone(), quic, name }))
}

/// The running binary, so new agents can be installed straight from the server.
async fn download_binary() -> R<Response> {
    let exe = std::env::current_exe().map_err(anyhow::Error::from)?;
    let f = tokio::fs::File::open(exe).await.map_err(anyhow::Error::from)?;
    Ok((
        [(header::CONTENT_TYPE, "application/octet-stream")],
        Body::from_stream(tokio_util::io::ReaderStream::new(f)),
    )
        .into_response())
}

async fn install_script(State(hub): S) -> Response {
    let url = hub.cfg.server.public_url.trim_end_matches('/').to_owned();
    let script = include_str!("../assets/install-agent.sh").replace("@@KILN_SERVER@@", &url);
    ([(header::CONTENT_TYPE, "text/x-shellscript")], script).into_response()
}

// ------------------------------------------------------------------ webhooks

async fn github_hook(State(hub): S, headers: HeaderMap, body: axum::body::Bytes) -> R<StatusCode> {
    // A configured but absent secret file means "no secret" (e.g. an optional mounted Secret).
    let secret = match &hub.cfg.gitops.webhook_secret_file {
        Some(path) => tokio::fs::read_to_string(path).await.ok().filter(|s| !s.trim().is_empty()),
        None => None,
    };
    if let Some(secret) = secret {
        let want = format!("sha256={}", crate::util::hmac_hex(secret.trim().as_bytes(), &body));
        let got = headers.get("x-hub-signature-256").and_then(|v| v.to_str().ok()).unwrap_or("");
        if !crate::util::ct_eq(want.as_bytes(), got.as_bytes()) {
            return Err(err(StatusCode::UNAUTHORIZED, "bad signature"));
        }
    }
    // A webhook only hints; the poll decides what changed (GitOps: state comes from git).
    hub.gitops_kick.notify_one();
    Ok(StatusCode::ACCEPTED)
}

// ------------------------------------------------------------------ events ws

async fn events_ws(State(hub): S, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |mut socket| async move {
        let m = crate::metrics::global();
        m.ws_clients.add(1);
        let mut rx = hub.events.subscribe();
        hub.emit_queue();
        loop {
            tokio::select! {
                ev = rx.recv() => match ev {
                    Ok(text) => {
                        if socket.send(Message::Text(text.as_ref().into())).await.is_err() { break; }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if socket.send(Message::Text(json!({"type": "resync"}).to_string().into())).await.is_err() { break; }
                    }
                    Err(_) => break,
                },
                msg = socket.recv() => match msg {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => {}
                },
            }
        }
        m.ws_clients.add(-1);
    })
}

// ------------------------------------------------------------------ terminal ws

#[derive(Deserialize)]
struct TermQ {
    target: Option<String>,
    cols: Option<u16>,
    rows: Option<u16>,
    cmd: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TermCtl {
    Resize { cols: u16, rows: u16 },
    Input { data: String },
    Ping,
}

async fn terminal_ws(State(hub): S, Query(q): Query<TermQ>, ws: WebSocketUpgrade) -> Response {
    if !hub.cfg.terminal.enabled {
        return err(StatusCode::FORBIDDEN, "terminal disabled in config").into_response();
    }
    if crate::metrics::global().terminal_sessions.get() >= i64::try_from(hub.cfg.terminal.max_sessions).unwrap_or(32) {
        return err(StatusCode::TOO_MANY_REQUESTS, "too many terminal sessions").into_response();
    }
    ws.on_upgrade(move |socket| async move {
        let target = q.target.clone().unwrap_or_else(|| "local".into());
        hub.store.audit("admin".into(), "terminal.open".into(), target.clone()).await;
        let r = match target.strip_prefix("agent:") {
            Some(agent) => agent_terminal(hub.clone(), agent.to_owned(), q, socket).await,
            None => local_terminal(hub.clone(), q, socket).await,
        };
        if let Err(e) = r {
            tracing::debug!(error = %e, "terminal session ended");
        }
    })
}

async fn local_terminal(hub: Arc<Hub>, q: TermQ, socket: WebSocket) -> anyhow::Result<()> {
    let (mut tx, mut rx) = socket.split();
    let (pty, mut out) = match crate::terminal::spawn(
        hub.cfg.terminal.shell.as_deref(),
        q.cmd.as_deref(),
        q.cols.unwrap_or(120),
        q.rows.unwrap_or(40),
    ) {
        Ok(p) => p,
        Err(e) => {
            tx.send(Message::Text(json!({"type": "error", "message": format!("{e:#}")}).to_string().into())).await?;
            return Ok(());
        }
    };
    loop {
        tokio::select! {
            o = out.recv() => match o {
                Some(crate::terminal::PtyOut::Data(d)) => tx.send(Message::Binary(d.into())).await?,
                Some(crate::terminal::PtyOut::Exit(code)) => {
                    tx.send(Message::Text(json!({"type": "exit", "code": code}).to_string().into())).await.ok();
                    return Ok(());
                }
                None => {
                    tx.send(Message::Text(json!({"type": "exit", "code": null}).to_string().into())).await.ok();
                    return Ok(());
                }
            },
            m = rx.next() => match m {
                Some(Ok(Message::Binary(b))) => pty.write(b.to_vec()),
                Some(Ok(Message::Text(t))) => match serde_json::from_str::<TermCtl>(&t) {
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

async fn agent_terminal(hub: Arc<Hub>, agent: String, q: TermQ, socket: WebSocket) -> anyhow::Result<()> {
    let (mut tx, mut rx) = socket.split();
    let session = crate::util::new_id();
    let key = format!("{agent}/{session}");
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<TermOut>(512);
    hub.terms.lock().insert(key.clone(), out_tx);
    let opened = hub
        .send_to_agent(
            &agent,
            LANE_TERM,
            &ServerMsg::TermOpen { session: session.clone(), cols: q.cols.unwrap_or(120), rows: q.rows.unwrap_or(40), command: q.cmd.clone() },
        )
        .await;
    if !opened {
        hub.terms.lock().remove(&key);
        tx.send(Message::Text(json!({"type": "error", "message": format!("agent {agent} is not connected")}).to_string().into())).await?;
        return Ok(());
    }
    crate::metrics::global().terminal_sessions.add(1);
    let result: anyhow::Result<()> = async {
        loop {
            tokio::select! {
                o = out_rx.recv() => match o {
                    Some(TermOut::Data(d)) => tx.send(Message::Binary(d.into())).await?,
                    Some(TermOut::Exit { code, error }) => {
                        let msg = match error {
                            Some(e) => json!({"type": "error", "message": e}),
                            None => json!({"type": "exit", "code": code}),
                        };
                        tx.send(Message::Text(msg.to_string().into())).await.ok();
                        return Ok(());
                    }
                    None => return Ok(()),
                },
                m = rx.next() => match m {
                    Some(Ok(Message::Binary(b))) => {
                        hub.send_to_agent(&agent, LANE_TERM, &ServerMsg::TermInput { session: session.clone(), data: b.to_vec() }).await;
                    }
                    Some(Ok(Message::Text(t))) => match serde_json::from_str::<TermCtl>(&t) {
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

// ------------------------------------------------------------------ router

async fn metrics_handler(State(hub): S, headers: HeaderMap) -> Response {
    if !hub.cfg.server.metrics_public && !authed(&hub, &headers) {
        return err(StatusCode::UNAUTHORIZED, "authentication required").into_response();
    }
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], crate::metrics::render()).into_response()
}

async fn healthz() -> &'static str {
    "ok"
}

async fn readyz(State(hub): S) -> Response {
    match hub.store.call(|c| Ok(c.query_row("SELECT 1", [], |r| r.get::<_, i64>(0))?)).await {
        Ok(_) => (StatusCode::OK, "ready").into_response(),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, format!("store: {e:#}")).into_response(),
    }
}

pub fn router(hub: Arc<Hub>) -> Router {
    let protected = Router::new()
        .route("/api/overview", get(overview))
        .route("/api/system", get(system))
        .route("/api/audit", get(audit))
        .route("/api/metrics/history", get(history))
        .route("/api/repos", get(repos).post(add_repo))
        .route("/api/repos/{id}", axum::routing::delete(delete_repo))
        .route("/api/repos/{id}/poll", post(poll_repo))
        .route("/api/repos/{id}/trigger", post(trigger))
        .route("/api/runs", get(runs))
        .route("/api/runs/{id}", get(run))
        .route("/api/runs/{id}/cancel", post(cancel))
        .route("/api/runs/{id}/retry", post(retry))
        .route("/api/jobs/{id}", get(job))
        .route("/api/jobs/{id}/logs", get(job_logs))
        .route("/api/jobs/{id}/logs/ws", get(job_logs_ws))
        .route("/api/jobs/{id}/artifacts", get(get_artifacts))
        .route("/api/agents", get(agents))
        .route("/api/agents/join", get(join_info))
        .route("/api/agents/{name}", axum::routing::delete(forget_agent))
        .route("/api/agents/{name}/drain", post(drain_agent))
        .route("/api/events", get(events_ws))
        .route("/api/terminal/ws", get(terminal_ws))
        .layer(middleware::from_fn_with_state(hub.clone(), require_auth));

    let open = Router::new()
        .route("/api/auth/login", post(login))
        .route("/api/auth/otp", get(otp))
        .route("/api/auth/logout", post(logout))
        .route("/api/me", get(me))
        .route("/api/agents/enroll", post(enroll))
        .route("/api/jobs/{id}/source", get(job_source))
        .route(
            "/api/jobs/{id}/artifacts/upload",
            put(put_artifacts).layer(DefaultBodyLimit::max(8 << 30)),
        )
        .route("/api/hooks/github", post(github_hook))
        .route("/metrics", get(metrics_handler))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/dl/kiln", get(download_binary))
        .route("/install.sh", get(install_script));

    Router::new()
        .merge(protected)
        .merge(open)
        .fallback(crate::ui::serve)
        .layer(tower_http::compression::CompressionLayer::new().gzip(true).br(true).zstd(true))
        .with_state(hub)
}
