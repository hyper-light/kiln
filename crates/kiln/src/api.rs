//! HTTP + WebSocket API for the UI and CLI (see docs/api.md), plus the small set of endpoints
//! agents use over HTTP: enrollment, join requests (`kiln join`), source archives and artifact
//! upload. Served by kiln's own HTTP layer (hyper's protocol code on hyper-rt).
//!
//! Auth is stateless so it survives restarts: sessions and one-time login links are HMACs of an
//! expiry under the admin token; job tokens are HMACs of the job id.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::StreamExt as _;
use http::{HeaderMap, Method, StatusCode, header};
use serde::Deserialize;
use serde_json::json;

use crate::agent::{EnrollRequest, EnrollResponse};
use crate::http::server::{self as srv, Request, Response};
use crate::http::ws::{self, Message};
use crate::hub::Hub;
use crate::logs::LogEvent;
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

/// The cookie a sign-in sets; `Secure` when the browser reached kiln over HTTPS.
fn session_cookie(hub: &Hub, secure: bool) -> String {
    cookie_for(&signed_expiring(hub, "session", SESSION_TTL), secure)
}

/// The session cookie carrying `value` (the owner's, or a user's: `crate::authz::gate`).
fn cookie_for(value: &str, secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!("{SESSION_COOKIE}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={SESSION_TTL}{secure}")
}

/// Who a request is: the owner (the admin token, the owner's session), else a token or a user's
/// session (docs/ci.md W10-R01).
async fn caller_of(hub: &Hub, headers: &HeaderMap) -> Option<crate::authz::Caller> {
    if authed(hub, headers) {
        return Some(crate::authz::Caller::owner());
    }
    let session = cookie(headers, SESSION_COOKIE);
    crate::authz::gate::caller(hub, bearer(headers), session.as_deref()).await
}

/// A `Host` header without its port (IPv6 literals keep their brackets).
fn host_only(h: &str) -> &str {
    let h = h.trim();
    if h.starts_with('[') {
        return h.split_inclusive(']').next().unwrap_or(h);
    }
    h.rsplit_once(':').filter(|(_, p)| p.bytes().all(|b| b.is_ascii_digit())).map_or(h, |(n, _)| n)
}

/// Whether the browser reached this server over HTTPS: the server's own TLS (`server.https`),
/// a runner's front door (the request is for a name one of them serves, see `crate::frontdoor`),
/// or a public URL that says https (TLS ends in front of kiln).
fn is_secure(hub: &Hub, req: &Request) -> bool {
    hub.cfg.server.public_url.starts_with("https")
        || req.extensions().get::<srv::Secure>().is_some()
        || srv::header_str(req, &header::HOST).is_some_and(|h| hub.front_doors.serves(host_only(h)))
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
    // A peered cluster's address (`peer-<slug>.<domain>`): every path there is the peer's.
    if let Some((f, slug)) = crate::peers::route(&hub, &req) {
        let secure = is_secure(&hub, &req);
        return Ok(f.serve(&slug, req, secure).await);
    }
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let seg = segments(&path);
    let s: Vec<&str> = seg.iter().map(String::as_str).collect();

    // Open routes (their own auth, if any).
    match (&method, s.as_slice()) {
        (&Method::POST, ["api", "mcp"]) => {
            // The owner, by the admin token, its session or an MCP token kiln minted; else a
            // user's session or token, whose operations the route table judges one by one.
            let caller = match crate::mcp::caller(&hub, req.headers(), authed(&hub, req.headers())) {
                Some(actor) => {
                    let mut c = crate::authz::Caller::owner();
                    c.name = actor;
                    Some(c)
                }
                None => caller_of(&hub, req.headers()).await,
            };
            return match caller {
                Some(caller) => crate::mcp::http(&hub, caller, req.into_body()).await.map_err(|(c, m)| err(c, m)),
                None => Err(err(StatusCode::UNAUTHORIZED, "authentication required")),
            };
        }
        (&Method::POST, ["api", "auth", "login"]) => return login(&hub, req).await,
        (&Method::GET, ["api", "auth", "otp"]) => return Ok(otp(&hub, &req).await),
        (&Method::GET, ["api", "auth", "methods"]) => return Ok(auth_methods(&hub).await),
        (&Method::POST, ["api", "auth", "password"]) => return login_password(&hub, req).await,
        (&Method::POST, ["api", "auth", "logout"]) => return Ok(logout()),
        (&Method::GET, ["api", "me"]) => return Ok(me(&hub, req.headers()).await),
        (&Method::POST, ["api", "agents", "enroll"]) => return enroll(&hub, req).await,
        (&Method::POST, ["api", "agents", "instance"]) => return enroll_instance(&hub, req).await,
        (&Method::GET, ["api", "agents", "tunnel"]) => return agent_tunnel(&hub, req),
        (&Method::POST, ["api", "join-requests"]) => return create_join(&hub, req).await,
        (&Method::GET, ["api", "join-requests", id]) => {
            let id = (*id).to_owned();
            return poll_join(&hub, &id, req.headers()).await;
        }
        (&Method::GET, ["api", "join", "invited"]) => return Ok(join_invited(&hub, &req)),
        (&Method::GET, ["api", "jobs", id, "source"]) => return job_source(&hub, id, req.headers()).await,
        (&Method::PUT, ["api", "jobs", id, "artifacts", "upload"]) => {
            let id = (*id).to_owned();
            return put_artifacts(&hub, &id, req).await;
        }
        (&Method::POST, ["api", "hooks", "github"]) => return github_hook(&hub, req).await,
        (&Method::GET, ["api", "shared", "plans", token]) => return shared_plan(&hub, token).await,
        (&Method::GET | &Method::HEAD, ["api", "sync", "blobs", hash]) => {
            let hash = (*hash).to_owned();
            return sync_blob_get(&hub, &hash, &req);
        }
        (&Method::PUT, ["api", "sync", "blobs", hash]) => {
            let hash = (*hash).to_owned();
            return sync_blob_put(&hub, &hash, req).await;
        }
        (&Method::GET, ["api", "sync", "bundles", batch]) => {
            let batch = (*batch).to_owned();
            return sync_bundle_get(&hub, &batch, &req);
        }
        (&Method::PUT, ["api", "sync", "bundles", batch]) => {
            let batch = (*batch).to_owned();
            return sync_bundle_put(&hub, &batch, req).await;
        }
        (&Method::GET, ["metrics"]) => return Ok(metrics_handler(&hub, req.headers())),
        (&Method::GET, ["healthz"]) => return Ok(srv::text(StatusCode::OK, "ok")),
        (&Method::GET, ["readyz"]) => return Ok(readyz(&hub).await),
        (&Method::GET, ["dl", "kiln"]) => return download_binary().await,
        (&Method::GET, ["dl", "kiln", target]) => return download_for(&hub, target).await,
        (&Method::GET, ["dl", "kiln", target, build]) => return download_build(&hub, target, build).await,
        // A machine building kiln itself (`crate::fleet`), with its build's token.
        (&Method::GET, ["api", "updates", "jobs", id, "source"]) => return build_source(&hub, id, req.headers()).await,
        (&Method::POST, ["api", "updates", "jobs", id, "binary"]) => {
            let id = (*id).to_owned();
            let q = srv::query(&req);
            return build_binary(&hub, &id, &q, req).await;
        }
        (&Method::GET, ["install.sh"]) => return Ok(install_script(&hub)),
        (&Method::GET, ["install.ps1"]) => return Ok(install_powershell(&hub)),
        // kiln's CA for devices (it is public): PEM, DER, and an Apple configuration profile.
        (&Method::GET, ["ca.pem"]) => return Ok(crate::frontdoor::control::ca(&hub, "pem")),
        (&Method::GET, ["ca.crt"]) => return Ok(crate::frontdoor::control::ca(&hub, "crt")),
        (&Method::GET, ["ca.mobileconfig"]) => return Ok(crate::frontdoor::control::ca(&hub, "mobileconfig")),
        _ => {}
    }
    // CI's open routes: runtime services, job ID tokens, the OIDC issuer (docs/ci.md §2.6).
    if crate::ci::api::is_open(&s) {
        return match crate::ci::api::open(&hub, &s, req).await {
            Some(r) => r.map_err(|(c, m)| err(c, m)),
            None => Err(err(StatusCode::NOT_FOUND, "not found")),
        };
    }

    let is_api = s.first() == Some(&"api");
    if !is_api {
        return Ok(crate::uiserver::serve(req).await);
    }
    let Some(caller) = caller_of(&hub, req.headers()).await else {
        return Err(err(StatusCode::UNAUTHORIZED, "authentication required"));
    };
    if method == Method::GET || method == Method::HEAD {
        return authorized(hub, caller, req).await;
    }
    // A change is audited (W19-R05): one row for the request, refused or not, which takes what
    // its handlers record on the way.
    let (who, q, ip) = (caller.clone(), srv::query(&req), srv::remote_ip(&req));
    let h = Arc::clone(&hub);
    crate::audit::request(&h, crate::audit::Req { caller: &who, method: &method, path: &path, q: &q, ip }, authorized(hub, caller, req), |r: &R| match r {
        Ok(resp) => resp.status().as_u16(),
        Err(e) => e.0.as_u16(),
    })
    .await
}

/// The API past authentication: everyone but the owner is held to the route table (W10-R05).
async fn authorized(hub: Arc<Hub>, caller: crate::authz::Caller, req: Request) -> R {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let seg = segments(&path);
    let s: Vec<&str> = seg.iter().map(String::as_str).collect();
    let q = srv::query(&req);
    if !caller.is_owner() {
        crate::authz::gate::check(&hub, &caller, &method, &s, &q).await.map_err(|(c, m)| err(c, m))?;
    }
    let mut req = req;
    req.extensions_mut().insert(caller.clone());
    if matches!(s.get(1), Some(&("users" | "grants" | "roles" | "tokens"))) {
        return match crate::authz::api::route(&hub, &caller, &method, &s, &q, req).await {
            Some(r) => r.map_err(|(c, m)| err(c, m)),
            None => Err(err(StatusCode::NOT_FOUND, "not found")),
        };
    }
    match (&method, s.as_slice()) {
        (&Method::GET, ["api", "openapi.json"]) => Ok(srv::json(StatusCode::OK, &crate::authz::openapi(env!("CARGO_PKG_VERSION")))),
        (&Method::GET, ["api", "peers"]) => crate::peers::list(&hub).await.map_err(|(c, m)| err(c, m)),
        (&Method::GET, ["api", "peers", "open"]) => crate::peers::openable(&hub).map_err(|(c, m)| err(c, m)),
        (&Method::POST, ["api", "peers"]) => crate::peers::register(&hub, &caller.name, req).await.map_err(|(c, m)| err(c, m)),
        (&Method::POST, ["api", "peers", "authorize"]) => crate::peers::authorize(&hub, &caller.name, req).await.map_err(|(c, m)| err(c, m)),
        (&Method::POST, ["api", "peers", id, "complete"]) => {
            let id = (*id).to_owned();
            crate::peers::complete(&hub, &caller.name, &id, req).await.map_err(|(c, m)| err(c, m))
        }
        (&Method::POST, ["api", "peers", id, "challenge"]) => crate::peers::rechallenge(&hub, &caller.name, id).await.map_err(|(c, m)| err(c, m)),
        (&Method::POST, ["api", "peers", id, "frame"]) => {
            let host = srv::header_str(&req, &header::HOST).unwrap_or_default().to_owned();
            crate::peers::frame(&hub, &caller, id, &host, is_secure(&hub, &req)).map_err(|(c, m)| err(c, m))
        }
        (&Method::DELETE, ["api", "peers", id]) => crate::peers::remove(&hub, &caller.name, id).await.map_err(|(c, m)| err(c, m)),
        (_, ["api", "cd", rest @ ..]) => crate::cd::api::route(&hub, rest, &q, req).await.map_err(|(c, m)| err(c, m)),
        (&Method::GET, ["api", "ci", "legs", id, "terminal"]) => {
            let id = (*id).to_owned();
            leg_terminal_ws(hub.clone(), &id, q, &caller, req).await
        }
        (_, ["api", "ci", rest @ ..]) => crate::ci::api::route(&hub, rest, &q, req).await.map_err(|(c, m)| err(c, m)),
        (&Method::GET, ["api", "overview"]) => overview(&hub).await,
        (&Method::GET, ["api", "system"]) => Ok(system(&hub).await),
        (&Method::GET, ["api", "audit"]) => audit(&hub, &q).await,
        (&Method::GET, ["api", "updates"]) => Ok(updates_overview(&hub, false).await),
        (&Method::POST, ["api", "updates", "check"]) => Ok(updates_overview(&hub, true).await),
        (&Method::POST, ["api", "updates", "runners"]) => Ok(upgrade_all_runners(&hub).await),
        (&Method::POST, ["api", "updates", "build"]) => build_every_platform(&hub).await,
        (&Method::POST, ["api", "updates", "runners", name]) => upgrade_runner(&hub, name).await,
        (&Method::PUT, ["api", "updates", "runners", name]) => set_runner_policy(&hub, name, req).await,
        (&Method::PUT, ["api", "updates", "policy"]) => set_policies(&hub, req).await,
        (&Method::POST, ["api", "updates", "server"]) => upgrade_server(&hub, req).await,
        (&Method::POST, ["api", "updates", "binaries"]) => upload_binary(&hub, &q, req).await,
        (&Method::DELETE, ["api", "updates", "binaries", target]) => delete_binary(&hub, target).await,
        (&Method::GET, ["api", "updates", "jobs", id]) => match hub.updates.job(id) {
            Some(j) => Ok(srv::json(StatusCode::OK, &j)),
            None => Err(err(StatusCode::NOT_FOUND, "no such upgrade")),
        },
        (&Method::GET, ["api", "metrics", "history"]) => Ok(history(&hub, &q)),
        (&Method::GET, ["api", "repos"]) => {
            let mut repos = hub.store.repos().await?;
            repos.retain(|r| caller.can("runs.read", &crate::authz::Scope::Repo { repo: r.name.clone() }));
            Ok(srv::json(StatusCode::OK, &repos))
        }
        (&Method::POST, ["api", "repos"]) => add_repo(&hub, req).await,
        (&Method::DELETE, ["api", "repos", id]) => delete_repo(&hub, id).await,
        (&Method::POST, ["api", "repos", id, "poll"]) => poll_repo(&hub, id).await,
        (&Method::POST, ["api", "repos", id, "trigger"]) => {
            let id = (*id).to_owned();
            trigger(&hub, &id, req).await
        }
        (&Method::GET, ["api", "runs"]) => runs(&hub, &q).await,
        (&Method::GET, ["api", "runs", id]) => run(&hub, id, &q).await,
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
        (&Method::GET, ["api", "reach", "runners"]) => {
            let want = srv::query(&req).get("want").cloned();
            reach_runners(&hub, want.as_deref())
        }
        (&Method::GET, ["api", "agents", "join"]) => Ok(join_info(&hub)),
        (&Method::POST, ["api", "outposts", "invite"]) => Ok(outpost_invite(&hub)),
        (&Method::GET, ["api", "discovery"]) => Ok(discovery(&hub).await),
        (&Method::POST, ["api", "discovery", "scan"]) => discovery_scan(&hub, req).await,
        (&Method::POST, ["api", "discovery", "probe"]) => discovery_probe(&hub, req).await,
        (&Method::POST, ["api", "discovery", "invite"]) => discovery_invite(&hub, req).await,
        (&Method::DELETE, ["api", "discovery", "invite", id]) => Ok(discovery_uninvite(&hub, id).await),
        (&Method::GET, ["api", "join-requests"]) => Ok(srv::json(StatusCode::OK, &hub.joins.list(Instant::now(), &hub.pki.ca_der))),
        (&Method::POST, ["api", "join-requests", id, "approve"]) => {
            let id = (*id).to_owned();
            approve_join(&hub, &id, req).await
        }
        (&Method::POST, ["api", "join-requests", id, "deny"]) => deny_join(&hub, id).await,
        (&Method::DELETE, ["api", "agents", name]) => forget_agent(&hub, name).await,
        (&Method::POST, ["api", "agents", name, "drain"]) => {
            let name = (*name).to_owned();
            drain_agent(&hub, &name, req).await
        }
        (&Method::GET, ["api", "front-doors"]) => front_door(crate::frontdoor::control::list(&hub).await),
        (&Method::GET, ["api", "runners", name, "front-door"]) => front_door(crate::frontdoor::control::get(&hub, name).await),
        (&Method::PUT, ["api", "runners", name, "front-door"]) => {
            let name = (*name).to_owned();
            front_door(crate::frontdoor::control::put(&hub, &name, req).await)
        }
        (&Method::POST, ["api", "runners", name, "front-door", "cert"]) => {
            let name = (*name).to_owned();
            front_door(crate::frontdoor::control::upload(&hub, &name, req).await)
        }
        (&Method::GET, ["api", "runners", name, "front-door", "cert"]) => front_door(crate::frontdoor::control::download(&hub, name).await),
        (&Method::POST, ["api", "runners", name, "front-door", "cert", "renew"]) => front_door(crate::frontdoor::control::renew(&hub, name).await),
        (&Method::GET, ["api", "tailscale"]) => front_door(crate::frontdoor::tailnet::get(&hub).await),
        (&Method::PUT, ["api", "tailscale"]) => front_door(crate::frontdoor::tailnet::put(&hub, req).await),
        (&Method::DELETE, ["api", "tailscale"]) => front_door(crate::frontdoor::tailnet::delete(&hub).await),
        (&Method::PUT, ["api", "settings", "password"]) => set_password(&hub, req).await,
        (&Method::DELETE, ["api", "settings", "password"]) => clear_password(&hub).await,
        (&Method::GET, ["api", "events"]) => events_ws(hub.clone(), req),
        (&Method::GET, ["api", "ai-sessions"]) => Ok(ai_sessions(&hub)),
        (&Method::GET, ["api", "ai-sessions", "silent"]) => Ok(ai_silent(&hub)),
        (&Method::GET, ["api", "ai-sessions", machine, pid, "transcript"]) => ai_transcript(&hub, machine, pid).await,
        (&Method::GET, ["api", "ai-sessions", machine, pid, "view"]) => ai_view(&hub, machine, pid).await,
        (&Method::POST, ["api", "ai-sessions", machine, pid, "takeover"]) => ai_takeover(&hub, machine, pid, &q, req).await,
        (&Method::POST, ["api", "ai-sessions", machine, pid, "fork"]) => ai_fork(&hub, machine, pid).await,
        (&Method::GET, ["api", "sync"]) => Ok(srv::json(StatusCode::OK, &hub.sync.view())),
        (&Method::POST, ["api", "sync"]) => sync_create(&hub, req).await,
        (&Method::DELETE, ["api", "sync", id]) => sync_ask(&hub, "sync.delete", id, crate::sync::server::Ask::Delete { repo: (*id).to_owned() }).await,
        (&Method::POST, ["api", "sync", id, "copies"]) => sync_add_copy(&hub, id, req).await,
        (&Method::DELETE, ["api", "sync", id, "copies", member]) => {
            let member: u64 = member.parse().map_err(|_| err(StatusCode::BAD_REQUEST, "bad member"))?;
            sync_ask(&hub, "sync.copy.remove", id, crate::sync::server::Ask::RemoveCopy { repo: (*id).to_owned(), member }).await
        }
        (&Method::POST, ["api", "sync", id, "conflicts", "resolve"]) => sync_resolve(&hub, id, req).await,
        (&Method::GET, ["api", "places"]) => Ok(srv::json(StatusCode::OK, &crate::sync::places::now(&hub).await?)),
        (&Method::POST, ["api", "places", "look"]) => Ok(places_look(&hub).await),
        (&Method::POST, ["api", "places", kind @ ("clone" | "sync" | "move" | "remove")]) => {
            let kind = (*kind).to_owned();
            places_start(&hub, &kind, req).await
        }
        (&Method::POST, ["api", "places", "tasks", id, "retry"]) => {
            let tasks = crate::sync::tasks::retry(&hub, id).await.map_err(|e| err(StatusCode::CONFLICT, e))?;
            hub.store.audit("admin".into(), "places.retry".into(), (*id).to_owned()).await;
            Ok(srv::json(StatusCode::CREATED, &tasks))
        }
        (&Method::DELETE, ["api", "places", "tasks", id]) => {
            if hub.tasks.dismiss(id) {
                hub.emit(&json!({ "type": "places" }));
                Ok(srv::status(StatusCode::NO_CONTENT))
            } else {
                Err(err(StatusCode::CONFLICT, "no such task, or it is still under way"))
            }
        }
        (&Method::GET, ["api", "plans"]) => plans_list(&hub, &q).await,
        (&Method::POST, ["api", "plans"]) => plans_create(&hub, req).await,
        (&Method::GET, ["api", "plans", id]) => crate::plans::get(&hub, id).await.map(|p| srv::json(StatusCode::OK, &p)).map_err(plan_err),
        (&Method::PATCH, ["api", "plans", id]) => plans_update(&hub, id, req).await,
        (&Method::DELETE, ["api", "plans", id]) => plans_delete(&hub, id).await,
        (&Method::GET, ["api", "plans", id, "versions"]) => crate::plans::versions(&hub, id).await.map(|v| srv::json(StatusCode::OK, &v)).map_err(plan_err),
        (&Method::GET, ["api", "plans", id, "runs"]) => crate::plans::runs(&hub, id).await.map(|v| srv::json(StatusCode::OK, &v)).map_err(plan_err),
        (&Method::POST, ["api", "plans", id, "restore"]) => plans_restore(&hub, id, req).await,
        (&Method::POST, ["api", "plans", id, "run"]) => plans_run(&hub, id, req).await,
        (&Method::POST, ["api", "plans", id, "share"]) => plans_share(&hub, id, req).await,
        (&Method::GET, ["api", "voice"]) => {
            Ok(srv::json(StatusCode::OK, &serde_json::json!({ "transcribe": hub.cfg.voice.enabled(), "language": hub.cfg.voice.language })))
        }
        (&Method::POST, ["api", "voice", "transcribe"]) => voice_transcribe(&hub, &q, req).await,
        (&Method::GET, ["api", "accounts"]) => accounts_status(&hub, &q).await,
        (&Method::POST, ["api", "accounts", "login"]) => accounts_login(&hub, req).await,
        (&Method::POST, ["api", "accounts", "logout"]) => accounts_logout(&hub, req).await,
        (&Method::GET, ["api", "accounts", "flows", id]) => {
            crate::accounts::get(id).map(|f| srv::json(StatusCode::OK, &f)).ok_or_else(|| err(StatusCode::NOT_FOUND, "no such sign-in"))
        }
        (&Method::POST, ["api", "accounts", "flows", id, "input"]) => accounts_input(id, req).await,
        (&Method::DELETE, ["api", "accounts", "flows", id]) => {
            crate::accounts::cancel(id).map(|()| srv::status(StatusCode::NO_CONTENT)).map_err(|e| err(StatusCode::NOT_FOUND, format!("{e:#}")))
        }
        (&Method::GET, ["api", "machines", target, "dirs"]) => machine_dirs(&hub, target, &q).await,
        (&Method::GET, ["api", "terminal", "sessions"]) => Ok(srv::json(StatusCode::OK, &hub.terminals.visible())),
        (&Method::POST, ["api", "terminal", "sessions"]) => create_terminal(&hub, req).await,
        (&Method::PUT, ["api", "terminal", "sessions", id, "name"]) => rename_terminal(&hub, id, req).await,
        (&Method::DELETE, ["api", "terminal", "sessions", id]) => {
            if let Some(s) = hub.terminals.get(id) {
                s.close();
                hub.store.audit("admin".into(), "terminal.close".into(), s.target.clone()).await;
            }
            Ok(srv::status(StatusCode::NO_CONTENT))
        }
        (&Method::GET, ["api", "terminal", "ws"]) => terminal_ws(hub.clone(), q, req).await,
        (&Method::GET, ["api", "harness", "kinds"]) => Ok(harness_kinds(&hub, &q)),
        (&Method::GET, ["api", "harness", "sessions"]) => Ok(srv::json(StatusCode::OK, &hub.harness.list())),
        (&Method::POST, ["api", "harness", "sessions"]) => harness_open(&hub, req).await,
        (&Method::GET, ["api", "harness", "sessions", id]) => match hub.harness.list().into_iter().find(|i| i.id == *id) {
            Some(info) => Ok(srv::json(StatusCode::OK, &info)),
            None => Err(err(StatusCode::NOT_FOUND, "no such agent session")),
        },
        (&Method::DELETE, ["api", "harness", "sessions", id]) => {
            let id = (*id).to_owned();
            if hub.harness.close(&hub, &id).await {
                let _ = crate::goals::forget(&hub, &id).await;
                hub.store.audit("admin".into(), "agent.close".into(), id).await;
                Ok(srv::status(StatusCode::NO_CONTENT))
            } else {
                Err(err(StatusCode::NOT_FOUND, "no such agent session"))
            }
        }
        (&Method::POST, ["api", "harness", "broadcast"]) => harness_broadcast(&hub, req).await,
        (&Method::POST, ["api", "harness", "broadcast", "preview"]) => harness_broadcast_preview(&hub, req).await,
        (&Method::POST, ["api", "harness", "machines", machine, "recover"]) => harness_recover(&hub, machine, req).await,
        (&Method::POST, ["api", "harness", "sessions", id, "move"]) => {
            let body: serde_json::Value = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
            let to = body.get("to").and_then(serde_json::Value::as_str).unwrap_or("").to_owned();
            let now = body.get("now").and_then(serde_json::Value::as_bool).unwrap_or(false);
            // Going on there in another harness, its conversation carried over (optional).
            let text = |k: &str| body.get(k).and_then(serde_json::Value::as_str).filter(|v| !v.is_empty()).map(str::to_owned);
            // Its machine away: from the server's copy of its conversation (`relocate::start`).
            match crate::harness::relocate::start(&hub, id, &to, now, text("kind"), text("model")).await {
                Ok(v) => Ok(srv::json(StatusCode::ACCEPTED, &v)),
                Err(e) => Err(err(StatusCode::CONFLICT, e)),
            }
        }
        (&Method::POST, ["api", "harness", "sessions", id, "command"]) => {
            let id = (*id).to_owned();
            harness_command(&hub, &id, req).await
        }
        (&Method::POST, ["api", "harness", "command"]) => harness_command_many(&hub, req).await,
        (&Method::POST, ["api", "harness", "files"]) => harness_upload(&hub, &q, req).await,
        (&Method::GET, ["api", "harness", "sessions", id, "ws"]) => harness_ws(hub.clone(), (*id).to_owned(), req),
        (&Method::GET, ["api", "harness", "sessions", id, "goals"]) => crate::goals::get(&hub, id).await.map(|g| srv::json(StatusCode::OK, &g)).map_err(goals_err),
        (&Method::PUT, ["api", "harness", "sessions", id, "goals"]) => goals_set(&hub, id, req).await,
        (&Method::POST, ["api", "harness", "sessions", id, "goals"]) => goals_add(&hub, id, req).await,
        (&Method::POST, ["api", "harness", "sessions", id, "goals", action]) => goals_control(&hub, id, action).await,
        (&Method::GET, ["api", "harness", "sessions", id, "terminals"]) => Ok(srv::json(StatusCode::OK, &crate::harness::screen::list(&hub, id))),
        (&Method::POST, ["api", "harness", "sessions", id, "terminals"]) => {
            let id = (*id).to_owned();
            harness_terminal(&hub, &id, req).await
        }
        (&Method::GET, ["api", "harness", "history"]) => harness_history(&hub, &q).await,
        (&Method::GET, ["api", "harness", "sessions", id, "events"]) => conversation_events(&hub, id, &q).await,
        (&Method::GET, ["api", "conversations", "search"]) => conversation_search(&hub, &q).await,
        // The runners that keep a copy of the server's conversation store (docs/conversations.md, "Mirrors").
        (&Method::GET, ["api", "conversations", "mirrors"]) => Ok(srv::json(StatusCode::OK, &json!({ "mirrors": hub.harness.copies.as_ref().map(|m| m.view()).unwrap_or_default() }))),
        (&Method::GET, ["api", "harness", "sessions", id, "tasks", task, "output"]) => {
            let since = q.get("since").and_then(|s| s.parse::<u64>().ok());
            match hub.harness.task_output(&hub, id, task, since).await {
                Ok(Some(v)) => Ok(srv::json(StatusCode::OK, &v)),
                Ok(None) => Err(err(StatusCode::NOT_FOUND, "no such agent session")),
                Err(e) => Err(err(StatusCode::BAD_GATEWAY, format!("{e:#}"))),
            }
        }
        _ => Err(err(StatusCode::NOT_FOUND, "not found")),
    }
}

/// A front-door handler's answer as the API's.
fn front_door(a: crate::frontdoor::control::Answer) -> R {
    a.map_err(|(code, msg)| err(code, msg))
}

// ------------------------------------------------------------------ AI sessions

/// The AI coding sessions running on every connected machine, most recently active first.
fn ai_sessions(hub: &Hub) -> Response {
    // Conversations kiln already runs as agent sessions (taken over, or resumed by hand there too).
    let managed: HashMap<String, String> = hub.harness.list().into_iter().filter_map(|i| Some((i.harness_session?, i.id))).collect();
    let mut out: Vec<serde_json::Value> = Vec::new();
    for (machine, sessions) in hub.ai_sessions.lock().iter() {
        for s in sessions {
            let mut v = serde_json::to_value(s).unwrap_or_default();
            if let Some(o) = v.as_object_mut() {
                o.insert("machine".into(), json!(machine));
                // Attachable only while kiln holds that terminal.
                let held = s.terminal.as_deref().is_some_and(|t| hub.terminals.get(t).is_some());
                if s.terminal.is_some() && !held {
                    o.insert("terminal".into(), serde_json::Value::Null);
                }
                o.insert("takeover".into(), json!(takeover_on(hub, machine, s.takeover(held))));
                o.insert("fork".into(), json!(s.forkable() && forks_on(hub, machine)));
                if let Some(agent) = managed.get(&s.id).filter(|_| !s.id.is_empty()) {
                    o.insert("managed_by".into(), json!(agent));
                }
            }
            out.push(v);
        }
    }
    out.sort_by_key(|v| std::cmp::Reverse(v.get("last_activity").and_then(serde_json::Value::as_i64).unwrap_or(0)));
    srv::json(StatusCode::OK, &out)
}

/// Take-over needs the machine's runner to wait for the old process (and to end one running
/// outside kiln); an older one would race it or refuse.
fn takeover_on(hub: &Hub, machine: &str, t: crate::aisessions::TakeOver) -> crate::aisessions::TakeOver {
    use crate::aisessions::TakeOver;
    let needs = match t {
        TakeOver::Ready => crate::proto::CAP_TAKEOVER,
        TakeOver::Stop => crate::proto::CAP_TAKEOVER_STOP,
        _ => return t,
    };
    if hub.runner_caps(machine) & needs == 0 { TakeOver::Upgrade } else { t }
}

/// A runner that lists folders ([`crate::proto::CAP_DIRS`]) is newer than `Spec::fork`: an older
/// one would resume the session itself, beside the one still running.
fn forks_on(hub: &Hub, machine: &str) -> bool {
    hub.runner_caps(machine) & crate::proto::CAP_DIRS != 0
}

/// Connected runners that have never reported AI sessions (an older kiln): what they run can't
/// be listed. A current runner reports right after it connects, even when it has none.
fn ai_silent(hub: &Hub) -> Response {
    let reporting: std::collections::HashSet<String> = hub.ai_sessions.lock().keys().cloned().collect();
    let mut machines: Vec<String> = hub
        .agents
        .lock()
        .values()
        .filter(|a| a.connected && !a.ephemeral && !reporting.contains(&a.name))
        .map(|a| a.name.clone())
        .collect();
    machines.sort();
    srv::json(StatusCode::OK, &json!({ "machines": machines }))
}

/// `POST /api/ai-sessions/:machine/:pid/takeover`.
#[derive(Deserialize, Default)]
struct TakeoverOpts {
    /// It runs outside kiln (`takeover: "stop"`): end it there. Asked for, never assumed.
    #[serde(default)]
    stop: bool,
}

/// Continue a session found running on a machine as a kiln agent session: the kiln terminal it
/// runs in is ended (or, asked with `{"stop": true}`, its process when it runs outside kiln), and
/// the harness resumes it there, in the same folder, once its process is gone (the machine waits
/// for it). An ended one is resumed with `POST /api/harness/sessions` and `spec.resume`; one left
/// running is branched with `.../fork`.
async fn ai_takeover(hub: &Hub, machine: &str, pid: &str, q: &HashMap<String, String>, req: Request) -> R {
    use crate::aisessions::TakeOver;
    let pid: u32 = pid.parse().map_err(|_| err(StatusCode::BAD_REQUEST, "bad pid"))?;
    let body = srv::body_bytes(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let opts: TakeoverOpts = if body.iter().all(u8::is_ascii_whitespace) {
        TakeoverOpts::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?
    };
    let stop = opts.stop || q.get("stop").is_some_and(|v| v == "1" || v == "true");
    let found = hub.ai_sessions.lock().get(machine).and_then(|l| l.iter().find(|s| s.pid == pid).cloned());
    let Some(found) = found else {
        return Err(err(StatusCode::NOT_FOUND, "no such session on that machine (it may have ended: resume it with spec.resume)"));
    };
    let held = found.terminal.as_deref().is_some_and(|t| hub.terminals.get(t).is_some());
    let spec = match (takeover_on(hub, machine, found.takeover(held)), found.takeover_spec(held)) {
        (TakeOver::Ready, Some(spec)) => spec,
        (TakeOver::Stop, Some(spec)) if stop => spec,
        (TakeOver::Stop, _) => {
            return Err(err(StatusCode::CONFLICT, format!("it runs outside kiln (process {pid}): taking it over ends it there, so ask with {{\"stop\": true}}")));
        }
        (TakeOver::Upgrade, _) => {
            return Err(err(StatusCode::CONFLICT, format!("{machine}'s runner is older than this take-over: upgrade it in Settings → Updates first")));
        }
        (TakeOver::Outside, _) => {
            return Err(err(StatusCode::CONFLICT, format!("it runs outside kiln (process {pid}), as a harness kiln might not resume: quit it there first, then continue it")));
        }
        (TakeOver::NoId, _) => return Err(err(StatusCode::CONFLICT, "it has no conversation to continue yet")),
        _ => return Err(err(StatusCode::BAD_REQUEST, format!("kiln cannot run {} as an agent session", found.tool))),
    };
    if let Some(t) = found.terminal.as_deref().and_then(|t| hub.terminals.get(t)) {
        t.close();
        hub.store.audit("admin".into(), "terminal.close".into(), t.target.clone()).await;
    }
    let how = if spec.takeover_stop { ", ended there" } else { "" };
    hub.store.audit("admin".into(), "agent.takeover".into(), format!("{} (process {pid}) on {machine}{how}", found.tool)).await;
    let infos = hub.harness.open(hub, &format!("agent:{machine}"), spec, 1).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    Ok(srv::json(StatusCode::OK, &infos))
}

/// Continue a copy of a session found running on a machine as a new kiln agent session (a branch
/// of its conversation, in the same folder), leaving the original running as it is.
async fn ai_fork(hub: &Hub, machine: &str, pid: &str) -> R {
    let pid: u32 = pid.parse().map_err(|_| err(StatusCode::BAD_REQUEST, "bad pid"))?;
    let found = hub.ai_sessions.lock().get(machine).and_then(|l| l.iter().find(|s| s.pid == pid).cloned());
    let Some(found) = found else {
        return Err(err(StatusCode::NOT_FOUND, "no such session on that machine (it may have ended: resume it with spec.resume)"));
    };
    if !forks_on(hub, machine) {
        return Err(err(StatusCode::CONFLICT, format!("{machine}'s runner is older than branching found sessions: upgrade it in Settings → Updates first")));
    }
    let Some(spec) = found.fork_spec() else {
        return Err(if !found.started() {
            err(StatusCode::CONFLICT, "it has no conversation to continue yet")
        } else {
            err(StatusCode::BAD_REQUEST, format!("kiln cannot branch a {} session", found.tool))
        });
    };
    hub.store.audit("admin".into(), "agent.fork".into(), format!("{} (process {pid}) on {machine}", found.tool)).await;
    let infos = hub.harness.open(hub, &format!("agent:{machine}"), spec, 1).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    Ok(srv::json(StatusCode::OK, &infos))
}

// ------------------------------------------------------------------ harness accounts

/// `POST /api/accounts/login` and `/api/accounts/logout`.
#[derive(Deserialize)]
struct AccountAsk {
    target: String,
    kind: String,
    #[serde(default)]
    method: Option<String>,
}

/// The harnesses' accounts on a machine (`?target=local|agent:<name>`), as their CLIs report.
async fn accounts_status(hub: &Arc<Hub>, q: &HashMap<String, String>) -> R {
    let target = q.get("target").map_or("local", String::as_str);
    let accounts = crate::accounts::status(hub, target).await.map_err(|e| err(StatusCode::CONFLICT, format!("{e:#}")))?;
    Ok(srv::json(StatusCode::OK, &accounts))
}

/// Start signing a harness in on a machine; follow it at `/api/accounts/flows/:id`.
async fn accounts_login(hub: &Arc<Hub>, req: Request) -> R {
    let a: AccountAsk = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let flow = crate::accounts::login(hub, &a.target, &a.kind, a.method.as_deref()).await.map_err(|e| err(StatusCode::CONFLICT, format!("{e:#}")))?;
    hub.store.audit("admin".into(), "account.login".into(), format!("{} on {} ({})", a.kind, a.target, flow.method.as_deref().unwrap_or("-"))).await;
    Ok(srv::json(StatusCode::OK, &flow))
}

/// Sign a harness out on a machine: the finished flow, with the account as it is now.
async fn accounts_logout(hub: &Arc<Hub>, req: Request) -> R {
    let a: AccountAsk = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    hub.store.audit("admin".into(), "account.logout".into(), format!("{} on {}", a.kind, a.target)).await;
    let flow = crate::accounts::logout(hub, &a.target, &a.kind).await.map_err(|e| err(StatusCode::CONFLICT, format!("{e:#}")))?;
    Ok(srv::json(StatusCode::OK, &flow))
}

#[derive(Deserialize)]
struct AccountInput {
    text: String,
}

/// Give a waiting sign-in what it asks for (the code its page showed, an API key): never logged,
/// nor shown again.
async fn accounts_input(id: &str, req: Request) -> R {
    let a: AccountInput = srv::body_json(req.into_body(), 16 * 1024).await.map_err(|_| err(StatusCode::BAD_REQUEST, "expected {\"text\": \"…\"}"))?;
    crate::accounts::input(id, &a.text).map_err(|e| err(StatusCode::CONFLICT, format!("{e:#}")))?;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

// ------------------------------------------------------------------ repo sync

/// Whether a blob request carries the token of the machine it names (`?machine=`), or an admin's.
fn sync_token_ok(hub: &Hub, req: &Request) -> bool {
    let q = srv::query(req);
    let machine = q.get("machine").map_or("", String::as_str);
    (!machine.is_empty() && bearer(req.headers()).is_some_and(|t| crate::util::ct_eq(t.as_bytes(), hub.sync.token(machine).as_bytes())))
        || authed(hub, req.headers())
}

/// One of the contents synced repos name (`HEAD`: whether the server has it).
fn sync_blob_get(hub: &Hub, hash: &str, req: &Request) -> R {
    if !sync_token_ok(hub, req) {
        return Err(err(StatusCode::UNAUTHORIZED, "bad sync token"));
    }
    let blobs = hub.sync.blobs().ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "sync is off"))?;
    let path = blobs.path(hash).filter(|p| p.is_file()).ok_or_else(|| err(StatusCode::NOT_FOUND, "no such blob"))?;
    if req.method() == Method::HEAD {
        return Ok(srv::status(StatusCode::OK));
    }
    let body = crate::rt::file_source(path).map_err(|_| err(StatusCode::NOT_FOUND, "no such blob"))?;
    Ok(srv::response(StatusCode::OK, "application/octet-stream", srv::channel_body(body)))
}

/// The largest contents a machine sends (a copy leaves out files over 256 MiB unless told otherwise).
const SYNC_BLOB_MAX: u64 = 1 << 30;
/// Room the server keeps free on its data volume: contents are refused below it.
const SYNC_DISK_FLOOR: u64 = 2 << 30;

/// Contents a machine's steps name: kept once they hash to their name. Refused past
/// [`SYNC_BLOB_MAX`], or while the server's disk has less than [`SYNC_DISK_FLOOR`] free.
async fn sync_blob_put(hub: &Hub, hash: &str, req: Request) -> R {
    if !sync_token_ok(hub, &req) {
        return Err(err(StatusCode::UNAUTHORIZED, "bad sync token"));
    }
    let blobs = hub.sync.blobs().ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "sync is off"))?;
    if !crate::sync::blobs::valid_hash(hash) {
        return Err(err(StatusCode::BAD_REQUEST, "not a SHA-256"));
    }
    if blobs.has(hash) {
        return Ok(srv::status(StatusCode::OK));
    }
    let length = req.headers().get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
    if length.is_some_and(|n| n > SYNC_BLOB_MAX) {
        return Err(err(StatusCode::PAYLOAD_TOO_LARGE, format!("contents are synced up to {} MiB", SYNC_BLOB_MAX >> 20)));
    }
    if blobs.free_bytes().is_some_and(|free| free < SYNC_DISK_FLOOR.saturating_add(length.unwrap_or(0))) {
        return Err(err(StatusCode::INSUFFICIENT_STORAGE, "the server's disk is nearly full: sync takes no more contents until there is room"));
    }
    let tmp = blobs.incoming();
    let (sink, done) = crate::rt::file_sink(tmp.clone(), SYNC_BLOB_MAX)?;
    let mut body = req.into_body();
    while let Some(chunk) = srv::next_chunk(&mut body).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))? {
        if sink.send(chunk).await.is_err() {
            break;
        }
    }
    drop(sink);
    if let Err(e) = done.await {
        let _ = std::fs::remove_file(&tmp);
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")));
    }
    let store = blobs.clone();
    let name = hash.to_owned();
    let kept = crate::rt::blocking(move || store.adopt(&tmp, &name)).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    match kept {
        Ok(true) => Ok(srv::status(StatusCode::CREATED)),
        Ok(false) => Err(err(StatusCode::BAD_REQUEST, "the contents don't hash to that name")),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

#[derive(Deserialize)]
struct SyncCopyAsk {
    machine: String,
    path: String,
}

#[derive(Deserialize)]
struct SyncCreateAsk {
    name: String,
    #[serde(default)]
    copies: Vec<SyncCopyAsk>,
    #[serde(default)]
    exclude: Vec<String>,
}

/// Machines kiln knows.
async fn known_machine(hub: &Hub, machine: &str) -> Result<(), ApiError> {
    if hub.agent_list().await.iter().any(|a| a.name == machine) {
        Ok(())
    } else {
        Err(err(StatusCode::BAD_REQUEST, format!("no machine named {machine}")))
    }
}

async fn sync_create(hub: &Hub, req: Request) -> R {
    let a: SyncCreateAsk = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    for c in &a.copies {
        known_machine(hub, &c.machine).await?;
    }
    let what = format!("{} ({} copies)", a.name, a.copies.len());
    let ask = crate::sync::server::Ask::Create { name: a.name, copies: a.copies.into_iter().map(|c| (c.machine, c.path)).collect(), exclude: a.exclude, transient: None };
    let id = hub.sync.ask(ask).await.map_err(|e| err(StatusCode::CONFLICT, e))?;
    hub.store.audit("admin".into(), "sync.create".into(), what).await;
    Ok(srv::json(StatusCode::CREATED, &serde_json::json!({ "id": id })))
}

async fn sync_add_copy(hub: &Hub, id: &str, req: Request) -> R {
    let a: SyncCopyAsk = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    known_machine(hub, &a.machine).await?;
    let what = format!("{id}: {}:{}", a.machine, a.path);
    let member = hub.sync.ask(crate::sync::server::Ask::AddCopy { repo: id.to_owned(), machine: a.machine, path: a.path, adopt: false }).await.map_err(|e| err(StatusCode::CONFLICT, e))?;
    hub.store.audit("admin".into(), "sync.copy.add".into(), what).await;
    Ok(srv::json(StatusCode::CREATED, &serde_json::json!({ "member": member.parse::<u64>().unwrap_or(0) })))
}

#[derive(Deserialize)]
struct SyncResolveAsk {
    path: String,
    keep: crate::sync::server::Keep,
}

async fn sync_resolve(hub: &Hub, id: &str, req: Request) -> R {
    let a: SyncResolveAsk = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let what = format!("{id}: {} ({:?})", a.path, a.keep);
    hub.sync.ask(crate::sync::server::Ask::Resolve { repo: id.to_owned(), path: a.path, keep: a.keep }).await.map_err(|e| err(StatusCode::CONFLICT, e))?;
    hub.store.audit("admin".into(), "sync.resolve".into(), what).await;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

async fn sync_ask(hub: &Hub, action: &str, id: &str, ask: crate::sync::server::Ask) -> R {
    hub.sync.ask(ask).await.map_err(|e| err(StatusCode::CONFLICT, e))?;
    hub.store.audit("admin".into(), action.to_owned(), id.to_owned()).await;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

// ------------------------------------------------------------------ repositories on machines

/// The largest bundle a machine sends (kiln's own mirrors' have no limit but the disk).
const SYNC_BUNDLE_MAX: u64 = 64 << 30;

/// A bundle's request: the machine it names (`?machine=`), when it carries that machine's token.
fn bundle_machine(hub: &Hub, req: &Request) -> Option<String> {
    let q = srv::query(req);
    let machine = q.get("machine").filter(|m| !m.is_empty())?.clone();
    bearer(req.headers()).is_some_and(|t| crate::util::ct_eq(t.as_bytes(), hub.sync.token(&machine).as_bytes())).then_some(machine)
}

/// A batch's bundle, for a machine it goes to.
fn sync_bundle_get(hub: &Hub, batch: &str, req: &Request) -> R {
    let machine = bundle_machine(hub, req).ok_or_else(|| err(StatusCode::UNAUTHORIZED, "bad sync token"))?;
    let (file, _, _) = hub.tasks.may_fetch(batch, &machine).ok_or_else(|| err(StatusCode::NOT_FOUND, "no such bundle for that machine"))?;
    let body = crate::rt::file_source(file).map_err(|_| err(StatusCode::NOT_FOUND, "no such bundle"))?;
    Ok(srv::response(StatusCode::OK, "application/octet-stream", srv::channel_body(body)))
}

/// A batch's bundle, from the machine that makes it: kept whole, hashed as it comes, refused while
/// the server's disk has less than [`SYNC_DISK_FLOOR`] free.
async fn sync_bundle_put(hub: &Hub, batch: &str, req: Request) -> R {
    use sha2::Digest as _;
    let machine = bundle_machine(hub, &req).ok_or_else(|| err(StatusCode::UNAUTHORIZED, "bad sync token"))?;
    if !hub.tasks.may_send(batch, &machine) {
        return Err(err(StatusCode::CONFLICT, format!("no bundle is wanted from {machine} for that")));
    }
    let file = hub.tasks.bundle_file(batch).ok_or_else(|| err(StatusCode::BAD_REQUEST, "no such batch"))?;
    let length = req.headers().get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
    if length.is_some_and(|n| n > SYNC_BUNDLE_MAX) {
        return Err(err(StatusCode::PAYLOAD_TOO_LARGE, format!("bundles go up to {} GiB", SYNC_BUNDLE_MAX >> 30)));
    }
    if hub.sync.blobs().and_then(crate::sync::Blobs::free_bytes).is_some_and(|free| free < SYNC_DISK_FLOOR.saturating_add(length.unwrap_or(0))) {
        return Err(err(StatusCode::INSUFFICIENT_STORAGE, "the server's disk is nearly full"));
    }
    let tmp = file.with_extension("part");
    let (sink, done) = crate::rt::file_sink(tmp.clone(), SYNC_BUNDLE_MAX)?;
    let mut h = sha2::Sha256::new();
    let mut body = req.into_body();
    while let Some(chunk) = srv::next_chunk(&mut body).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))? {
        h.update(&chunk);
        if sink.send(chunk).await.is_err() {
            break;
        }
    }
    drop(sink);
    let size = match done.await {
        Ok(n) => n,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")));
        }
    };
    std::fs::rename(&tmp, &file).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let hash = hex::encode(h.finalize());
    hub.tasks.sent(batch, hash.clone(), size);
    Ok(srv::json(StatusCode::CREATED, &json!({ "hash": hash, "size": size })))
}

/// Starts a clone, sync, move or removal (`kind`) of a repository: its tasks, under way.
async fn places_start(hub: &Arc<Hub>, kind: &str, req: Request) -> R {
    let mut v: serde_json::Value = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    if let Some(o) = v.as_object_mut() {
        o.insert("kind".into(), json!(kind));
    }
    let ask: crate::sync::tasks::Ask = serde_json::from_value(v).map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    let what = format!("{} to {}", ask.key, ask.to.iter().map(|t| format!("{}:{}", t.machine, t.path)).collect::<Vec<_>>().join(", "));
    let tasks = crate::sync::tasks::start(hub, ask).await.map_err(|e| err(StatusCode::CONFLICT, e))?;
    hub.store.audit("admin".into(), format!("places.{kind}"), what).await;
    Ok(srv::json(StatusCode::CREATED, &tasks))
}

/// Asks every connected machine that can to look at its repositories again now.
async fn places_look(hub: &Hub) -> Response {
    let mut asked = 0;
    for a in hub.agent_list().await.into_iter().filter(|a| a.connected && !a.ephemeral) {
        if hub.runner_caps(&a.name) & crate::proto::CAP_REPOS != 0 {
            hub.sync.send(&a.name, crate::sync::wire::Msg::Look);
            asked += 1;
        }
    }
    srv::json(StatusCode::ACCEPTED, &json!({ "asked": asked }))
}

// ------------------------------------------------------------------ plans (crate::plans)

fn plan_err(e: crate::plans::Error) -> ApiError {
    match e {
        crate::plans::Error::Bad(m) => err(StatusCode::BAD_REQUEST, m),
        crate::plans::Error::Missing => err(StatusCode::NOT_FOUND, "no such plan"),
        crate::plans::Error::Other(e) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

async fn plans_list(hub: &Hub, q: &HashMap<String, String>) -> R {
    let plans = crate::plans::list(hub, q.get("q").cloned(), q.get("tag").cloned()).await.map_err(plan_err)?;
    Ok(srv::json(StatusCode::OK, &plans))
}

async fn plans_create(hub: &Hub, req: Request) -> R {
    let n: crate::plans::New = srv::body_json(req.into_body(), 512 * 1024).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let p = crate::plans::create(hub, n, "admin").await.map_err(plan_err)?;
    hub.store.audit("admin".into(), "plan.create".into(), p.title.clone()).await;
    Ok(srv::json(StatusCode::CREATED, &p))
}

async fn plans_update(hub: &Hub, id: &str, req: Request) -> R {
    let patch: crate::plans::Patch = srv::body_json(req.into_body(), 512 * 1024).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let p = crate::plans::update(hub, id, patch, "admin").await.map_err(plan_err)?;
    hub.store.audit("admin".into(), "plan.update".into(), format!("{} (v{})", p.title, p.version)).await;
    Ok(srv::json(StatusCode::OK, &p))
}

async fn plans_delete(hub: &Hub, id: &str) -> R {
    crate::plans::delete(hub, id).await.map_err(plan_err)?;
    hub.store.audit("admin".into(), "plan.delete".into(), id.to_owned()).await;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

#[derive(Deserialize)]
struct PlanRestore {
    version: i64,
}

async fn plans_restore(hub: &Hub, id: &str, req: Request) -> R {
    let r: PlanRestore = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let p = crate::plans::restore(hub, id, r.version, "admin").await.map_err(plan_err)?;
    hub.store.audit("admin".into(), "plan.restore".into(), format!("{} (v{} as v{})", p.title, r.version, p.version)).await;
    Ok(srv::json(StatusCode::OK, &p))
}

#[derive(Deserialize)]
struct PlanRun {
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    new: Option<PlanRunNew>,
    #[serde(default)]
    mode: crate::plans::Mode,
}

#[derive(Deserialize)]
struct PlanRunNew {
    #[serde(default)]
    target: Option<String>,
    kind: String,
    #[serde(default)]
    cwd: Option<String>,
}

async fn plans_run(hub: &Hub, id: &str, req: Request) -> R {
    let r: PlanRun = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let target = match (r.session, r.new) {
        (Some(s), _) => crate::plans::Target::Session(s),
        (None, Some(n)) => crate::plans::Target::New { target: n.target.unwrap_or_else(|| "local".into()), kind: n.kind, cwd: n.cwd },
        (None, None) => return Err(err(StatusCode::BAD_REQUEST, "say where to run it: an agent (`session`) or a new one (`new`)")),
    };
    let ran = crate::plans::run(hub, id, target, r.mode, "admin").await.map_err(plan_err)?;
    hub.store.audit("admin".into(), "plan.run".into(), format!("{id} on {} ({} messages)", ran.session, ran.sent)).await;
    Ok(srv::json(StatusCode::OK, &ran))
}

#[derive(Deserialize)]
struct PlanShare {
    on: bool,
}

async fn plans_share(hub: &Hub, id: &str, req: Request) -> R {
    let r: PlanShare = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let p = crate::plans::share(hub, id, r.on).await.map_err(plan_err)?;
    hub.store.audit("admin".into(), if r.on { "plan.share" } else { "plan.unshare" }.into(), p.title.clone()).await;
    Ok(srv::json(StatusCode::OK, &p))
}

/// A shared plan, for anyone with its link: its words only, never where it came from.
async fn shared_plan(hub: &Hub, token: &str) -> R {
    if token.len() < 16 {
        return Err(err(StatusCode::NOT_FOUND, "no such plan"));
    }
    let p = crate::plans::shared(hub, token).await.map_err(plan_err)?;
    Ok(srv::json(StatusCode::OK, &json!({ "title": p.title, "body": p.body, "items": p.items, "steps": p.steps, "version": p.version, "updated_at": p.updated_at })))
}

// ------------------------------------------------------------------ voice

/// Speech to text for the console's microphone where the browser can't do it itself: the body is
/// the recording (`audio/*`, as `MediaRecorder` made it), `?lang=` overrides `[voice] language`;
/// answers `{text}`.
async fn voice_transcribe(hub: &Hub, q: &HashMap<String, String>, req: Request) -> R {
    if !hub.cfg.voice.enabled() {
        return Err(err(StatusCode::NOT_IMPLEMENTED, "voice transcription isn't set up on this server: set [voice] transcribe_url to a speech-to-text service (docs/voice.md)"));
    }
    let kind = req.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
    if !kind.starts_with("audio/") {
        return Err(err(StatusCode::UNSUPPORTED_MEDIA_TYPE, format!("expected a recording (audio/*), not {kind:?}")));
    }
    let max = crate::voice::MAX_AUDIO;
    let audio = srv::body_bytes(req.into_body(), max).await.map_err(|e| err(StatusCode::PAYLOAD_TOO_LARGE, format!("a recording is up to {} MiB ({e:#})", max >> 20)))?;
    if audio.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "the recording is empty"));
    }
    let text = crate::voice::transcribe(&hub.cfg.voice, &audio, &kind, q.get("lang").map(String::as_str)).await.map_err(|e| err(StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    Ok(srv::json(StatusCode::OK, &serde_json::json!({ "text": text })))
}

/// The recent messages of one session, read from its machine.
/// A session running elsewhere, as kiln's normalized view: events, state and changeset.
async fn ai_view(hub: &Hub, machine: &str, pid: &str) -> R {
    let pid: u32 = pid.parse().map_err(|_| err(StatusCode::BAD_REQUEST, "bad pid"))?;
    match hub.ai_view(machine, pid).await {
        Ok(Some(json)) => Ok(srv::response(StatusCode::OK, "application/json", srv::full(json))),
        Ok(None) => Err(err(StatusCode::NOT_FOUND, "no view for that session (not Claude Code, or no transcript)")),
        Err(e) => Err(err(StatusCode::BAD_GATEWAY, format!("{e:#}"))),
    }
}

async fn ai_transcript(hub: &Hub, machine: &str, pid: &str) -> R {
    let pid: u32 = pid.parse().map_err(|_| err(StatusCode::BAD_REQUEST, "bad pid"))?;
    match hub.ai_transcript(machine, pid).await {
        Ok(Some(json)) => {
            let entries: serde_json::Value = serde_json::from_str(&json).map_err(|e| err(StatusCode::BAD_GATEWAY, format!("unreadable transcript: {e}")))?;
            Ok(srv::json(StatusCode::OK, &entries))
        }
        Ok(None) => Err(err(StatusCode::NOT_FOUND, "that session has no transcript")),
        Err(e) => Err(err(StatusCode::BAD_GATEWAY, format!("{e:#}"))),
    }
}

// ------------------------------------------------------------------ auth handlers

#[derive(Deserialize)]
struct Login {
    token: String,
}

async fn login(hub: &Hub, req: Request) -> R {
    let secure = is_secure(hub, &req);
    let l: Login = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    if crate::util::ct_eq(l.token.trim().as_bytes(), hub.admin_token.as_bytes()) {
        hub.store.audit("admin".into(), "login".into(), "token".into()).await;
        Ok(srv::with_header(srv::status(StatusCode::NO_CONTENT), header::SET_COOKIE, &session_cookie(hub, secure)))
    } else {
        crate::rt::sleep(Duration::from_millis(500)).await;
        Err(err(StatusCode::UNAUTHORIZED, "invalid token"))
    }
}

async fn otp(hub: &Hub, req: &Request) -> Response {
    let q = srv::query(req);
    let code = q.get("code").map(String::as_str).unwrap_or("");
    // A user's link (`u.<id>.<secret>`): theirs, once.
    if code.starts_with("u.") {
        return match crate::authz::gate::redeem_link(hub, code).await {
            Some((session, name)) => {
                hub.store.audit(format!("user:{name}"), "login".into(), "sign-in link".into()).await;
                let r = srv::with_header(srv::status(StatusCode::SEE_OTHER), header::LOCATION, "/");
                srv::with_header(r, header::SET_COOKIE, &cookie_for(&session, is_secure(hub, req)))
            }
            None => srv::with_header(srv::status(StatusCode::SEE_OTHER), header::LOCATION, "/?signin=expired"),
        };
    }
    if verify_expiring(hub, "otp", code) {
        let hub_store = hub.store.clone();
        crate::rt::spawn(async move {
            hub_store.audit("admin".into(), "login".into(), "one-time link".into()).await;
        });
        let r = srv::with_header(srv::status(StatusCode::SEE_OTHER), header::LOCATION, "/");
        srv::with_header(r, header::SET_COOKIE, &session_cookie(hub, is_secure(hub, req)))
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
    let secure = is_secure(hub, &req);
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
            Ok(srv::with_header(srv::status(StatusCode::NO_CONTENT), header::SET_COOKIE, &session_cookie(hub, secure)))
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

async fn me(hub: &Hub, headers: &HeaderMap) -> Response {
    let caller = caller_of(hub, headers).await;
    // What discovery announces anyway (when it does), so a probe can tell which instance answers.
    let d = &hub.discovery;
    let announced = d.announcing().then(|| {
        let a = d.advert.lock();
        json!({
            "id": a.id, "instance": a.instance_label(), "name": a.name, "role": a.role, "version": a.version,
            "port": a.port, "quic": a.quic, "ca": a.ca, "os": a.os, "arch": a.arch,
        })
    });
    srv::json(
        StatusCode::OK,
        &json!({
            "authenticated": caller.is_some(),
            // Who (W10-R01): `admin` for the owner, `user:<name>`, a token, a job; and what they
            // hold, when that isn't everything.
            "principal": caller.as_ref().map(|c| c.name.clone()),
            "kind": caller.as_ref().map(|c| c.kind),
            "grants": caller.as_ref().and_then(crate::authz::Caller::grants),
            "public_url": hub.cfg.server.public_url,
            // Where else browsers find this server: its runners' front doors (HTTPS).
            "origins": hub.front_doors.origins(),
            "version": env!("CARGO_PKG_VERSION"),
            "hostname": crate::config::Config::hostname(),
            "discovery": announced,
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
    Ok(srv::json(StatusCode::OK, &crate::audit::list(&hub.store, q).await?))
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

async fn run(hub: &Hub, id: &str, q: &HashMap<String, String>) -> R {
    let run = hub.store.run(id.to_owned()).await?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such run"))?;
    // A CI run's attempt (docs/ci.md W05-R17); the current one by default.
    let attempt = q.get("attempt").and_then(|a| a.parse().ok());
    let jobs = hub.store.jobs_at(id.to_owned(), false, attempt).await?;
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
    let mut data = hub.logs.read(id, step).await;
    // A CI leg a rerun kept: its original's logs (docs/ci.md W05-R17).
    if data.is_empty()
        && let Some(from) = hub.store.reused_from(id.to_owned()).await?
    {
        data = hub.logs.read(&from, step).await;
    }
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

pub(crate) fn artifact_path(hub: &Hub, job: &str) -> std::path::PathBuf {
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
            "macos": format!("curl -fsSL {url}/install.sh | sh -s -- --token {token} --labels macos"),
            "windows": format!("& ([scriptblock]::Create((irm '{url}/install.ps1'))) -Token '{token}' -Labels 'windows'"),
            "manual": format!("kiln agent --server {url} --token {token} --labels baremetal,bench"),
            // No token: the machine asks to join and an admin approves it (verification code).
            "join": format!("curl -fsSL {url}/install.sh | sh -s -- --join"),
            "join_manual": format!("kiln join --server {url}"),
            // Windows: install.ps1 without a token asks to join the same way.
            "join_windows": format!("irm '{url}/install.ps1' | iex"),
            // One line per OS that downloads kiln from this server and installs the agent with the
            // join token (`install.sh` passes `--token`/`--labels` to `kiln install --agent`).
            "install": {
                "linux": format!("curl -fsSL {url}/install.sh | sh -s -- --token {token} --labels baremetal,linux"),
                "macos": format!("curl -fsSL {url}/install.sh | sh -s -- --token {token} --labels baremetal,macos"),
                "windows": format!("& ([scriptblock]::Create((irm '{url}/install.ps1'))) -Token '{token}' -Labels 'baremetal,windows'"),
            },
            // The join token is kept in <data>/join.token and does not expire.
            "expires_at": serde_json::Value::Null,
        }),
    )
}

/// `POST /api/outposts/invite`: a one-time invite for an outpost (`kiln outpost`), and a line per
/// OS that downloads kiln from this server into a folder of its own and runs it from there,
/// installing nothing. The invite works once, within [`crate::hub::OUTPOST_INVITE_TTL`]. `ask` is
/// the same without the invite: the outpost asks to join, and an admin approves it.
fn outpost_invite(hub: &Hub) -> Response {
    let url = hub.cfg.server.public_url.trim_end_matches('/').to_owned();
    let (token, expires_at) = hub.outpost_invites.mint();
    srv::json(
        StatusCode::OK,
        &json!({
            "token": token,
            "expires_at": expires_at,
            "commands": {
                "linux": format!("curl -fsSL {url}/install.sh | sh -s -- --outpost --token {token}"),
                "macos": format!("curl -fsSL {url}/install.sh | sh -s -- --outpost --token {token}"),
                "windows": format!("& ([scriptblock]::Create((irm '{url}/install.ps1'))) -Outpost -Token '{token}'"),
            },
            "ask": {
                "linux": format!("curl -fsSL {url}/install.sh | sh -s -- --outpost"),
                "macos": format!("curl -fsSL {url}/install.sh | sh -s -- --outpost"),
                "windows": format!("& ([scriptblock]::Create((irm '{url}/install.ps1'))) -Outpost"),
            },
        }),
    )
}

async fn enroll(hub: &Hub, req: Request) -> R {
    let r: EnrollRequest = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    let persistent = crate::util::ct_eq(r.token.as_bytes(), hub.join_token.as_bytes());
    let ephemeral = !persistent && hub.ephemeral_tokens.lock().remove(&r.token).is_some();
    let outpost = !persistent && !ephemeral && hub.outpost_invites.take(&r.token);
    if !persistent && !ephemeral && !outpost {
        crate::rt::sleep(Duration::from_millis(500)).await;
        return Err(err(StatusCode::UNAUTHORIZED, "invalid join token"));
    }
    let name = crate::pki::sanitize_name(&r.name);
    // An outpost never takes another machine's name: the same machine enrolling again keeps its
    // own, another one gets `<name>-2`.
    let name = if outpost { free_agent_name(hub, &name, &r.hostname).await } else { name };
    let days = if ephemeral {
        crate::pki::EPHEMERAL_DAYS
    } else if outpost {
        crate::pki::OUTPOST_DAYS
    } else {
        365
    };
    let cert = hub.pki.sign_csr(&r.csr, &name, days).map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let quic = if ephemeral {
        hub.cfg.kubernetes.server_quic.clone().unwrap_or_else(|| hub.cfg.server.quic_advertise.clone())
    } else {
        hub.cfg.server.quic_advertise.clone()
    };
    if persistent || outpost {
        let what = if outpost { "outpost.enroll" } else { "agent.enroll" };
        hub.store.audit(name.clone(), what.into(), String::new()).await;
        tracing::info!(agent = %name, outpost, "agent enrolled");
    }
    Ok(srv::json(StatusCode::OK, &EnrollResponse { cert_pem: cert, ca_pem: hub.pki.ca_pem.clone(), quic, name }))
}

/// `GET /api/reach/runners?want=linux/amd64`: reach's directory, ranked for `want` (every runner
/// without one), with why the others were passed over.
fn reach_runners(hub: &Hub, want: Option<&str>) -> R {
    let runners = hub.reach_directory();
    let want = match want.map(crate::reach::directory::Want::parse).transpose() {
        Ok(w) => w.unwrap_or_default(),
        Err(e) => return Err(err(StatusCode::BAD_REQUEST, format!("{e:#}"))),
    };
    let (ranked, passed) = crate::reach::directory::rank(&runners, &want, "");
    Ok(srv::json(StatusCode::OK, &json!({ "want": want.describe(), "runners": runners, "ranked": ranked, "passed": passed })))
}

/// `POST /api/agents/instance`: a certificate for a second runner process of a machine that is
/// already enrolled, so a new runner can work beside the old one (which keeps its terminals until
/// they end) instead of replacing it. Needs no token: the request carries the machine's current
/// certificate and a CSR signed with that certificate's key ([`crate::pki::Pki::sign_instance`]).
async fn enroll_instance(hub: &Hub, req: Request) -> R {
    let r: crate::agent::InstanceRequest =
        srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    let (name, cert) = match hub.pki.sign_instance(&r.cert_pem, &r.csr) {
        Ok(v) => v,
        Err(e) => {
            crate::rt::sleep(Duration::from_millis(500)).await;
            return Err(err(StatusCode::FORBIDDEN, format!("{e:#}")));
        }
    };
    // Only machines this server still knows: a forgotten (revoked) one must enroll again.
    let known = hub.store.agents().await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?.iter().any(|a| a.name == name);
    if !known {
        return Err(err(StatusCode::FORBIDDEN, format!("{name} is not enrolled here any more; enroll it again")));
    }
    hub.store.audit(name.clone(), "agent.instance".into(), String::new()).await;
    tracing::info!(agent = %name, "issued a certificate for a second runner process");
    Ok(srv::json(
        StatusCode::OK,
        &crate::agent::EnrollResponse { cert_pem: cert, ca_pem: hub.pki.ca_pem.clone(), quic: hub.cfg.server.quic_advertise.clone(), name },
    ))
}

/// The target triple this binary was built for (`build.rs`).
pub const TARGET: &str = env!("KILN_TARGET");

/// The commit this binary was built from (`build.rs`); empty when it was built without knowing
/// one (a tree with changes, or a Docker build without `--build-arg KILN_COMMIT=...`).
pub const COMMIT: &str = env!("KILN_COMMIT");

/// Where the release workflow (`.github/workflows/release.yml`) publishes binaries: the asset
/// `kiln-<version>-<target>` (`.exe` on Windows) of release `v<version>`.
pub const RELEASES: &str = "https://github.com/hyper-light/kiln/releases/download";

/// The targets the release workflow builds.
pub const RELEASE_TARGETS: [&str; 6] = [
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
];

/// The URL of this version's release binary for `target`.
pub fn release_url(target: &str) -> String {
    let version = env!("CARGO_PKG_VERSION");
    let exe = if target.contains("-windows-") { ".exe" } else { "" };
    format!("{RELEASES}/v{version}/kiln-{version}-{target}{exe}")
}

// ------------------------------------------------------------------ discovery & joining

/// Instances as the API lists them: each marked with whether it already is one of this server's
/// agents, the command that would make it one, whether this server invited it and its pending join
/// request.
async fn render_instances(hub: &Hub, list: &[crate::discovery::Instance]) -> Vec<serde_json::Value> {
    let d = &hub.discovery;
    let agents = hub.agent_list().await;
    let ours = crate::discovery::ca_fingerprint(&hub.pki.ca_der);
    let now = Instant::now();
    let all = d.table.snapshot();
    let requests = hub.joins.list(now, &hub.pki.ca_der);
    list.iter()
        .map(|i| {
            let agent = joined_as(&agents, i, &ours);
            let url = our_url_for(hub, i);
            let mut v = serde_json::to_value(i).unwrap_or_default();
            if let Some(o) = v.as_object_mut() {
                o.insert("url".into(), json!(i.url()));
                o.insert("joined".into(), json!(agent.is_some()));
                o.insert("agent".into(), json!(agent));
                o.insert("same_ca".into(), json!(i.ca.as_deref() == Some(ours.as_str())));
                o.insert("join_command".into(), json!(format!("kiln join --server {url}")));
                o.insert("invited".into(), json!(d.invites.invited(&i.id, now)));
                o.insert("request".into(), json!(crate::discovery::invite::request_for(i, &all, &requests)));
            }
            v
        })
        .collect()
}

/// The kiln instances found, how each way of finding them is doing, what is wrong, the latest
/// scan, and unicast DNS-SD's domains, publishing and zone-file lines.
async fn discovery(hub: &Hub) -> Response {
    let d = &hub.discovery;
    let me = d.advert.lock().clone();
    let instances = render_instances(hub, &d.table.snapshot()).await;
    let health = d.health.lock().clone();
    // Until the first browse ran: what it is going to browse.
    let domains = if health.dns_sd.ran {
        health.dns_sd.domains.clone()
    } else {
        let search = crate::discovery::unicast::system_resolver().1;
        let mut v: Vec<String> = Vec::new();
        for s in d.settings.domains.iter().chain(&search) {
            if let Some(n) = crate::discovery::unicast::browsable(s).map(|n| n.to_zone(false))
                && !v.contains(&n)
            {
                v.push(n);
            }
        }
        v
    };
    srv::json(
        StatusCode::OK,
        &json!({
            "enabled": d.settings.enabled,
            "announce": d.settings.enabled && d.settings.announce,
            "browse": d.settings.enabled && d.settings.browse,
            "join_requests": d.settings.join_requests,
            "self": {
                "id": me.id,
                "name": me.name,
                "instance": me.instance_label(),
                "ca": me.ca,
                "port": me.port,
                "quic": me.quic,
                "version": me.version,
            },
            "instances": instances,
            "methods": d.methods(),
            "problems": d.problems(),
            "subnets": crate::discovery::subnets(&d.settings.interfaces),
            "scan": d.probes.scan(),
            "dns_sd": {
                "domains": domains,
                "publish": health.publish,
                "records": crate::discovery::publish::manual_records(d),
            },
        }),
    )
}

/// Tells UI clients that discovery changed (scans run on after their request).
fn discovery_events(hub: &Arc<Hub>) -> Arc<dyn Fn() + Send + Sync> {
    let weak = Arc::downgrade(hub);
    Arc::new(move || {
        if let Some(h) = weak.upgrade() {
            h.emit(&json!({ "type": "discovery" }));
        }
    })
}

#[derive(Deserialize, Default)]
struct ScanRequest {
    #[serde(default)]
    cidrs: Option<Vec<String>>,
}

/// `POST /api/discovery/scan`: probe every address of the given ranges (default: the LAN subnets,
/// each at most a /22) in the background; 202 with the scan's state, progress as events.
async fn discovery_scan(hub: &Arc<Hub>, req: Request) -> R {
    let body: ScanRequest = srv::body_json(req.into_body(), JSON_LIMIT).await.unwrap_or_default();
    let state = crate::discovery::probe::start_scan(&hub.discovery, body.cidrs, discovery_events(hub)).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    hub.store.audit("admin".into(), "discovery.scan".into(), state.cidrs.join(", ")).await;
    hub.emit(&json!({ "type": "discovery" }));
    Ok(srv::json(StatusCode::ACCEPTED, &state))
}

#[derive(Deserialize)]
struct ProbeRequest {
    address: String,
}

/// `POST /api/discovery/probe`: one address or host name (optionally `:port`), by unicast multicast
/// DNS and `GET /api/me`.
async fn discovery_probe(hub: &Arc<Hub>, req: Request) -> R {
    let body: ProbeRequest = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    let events = discovery_events(hub);
    let found = crate::discovery::probe::probe(&hub.discovery, &body.address, &*events).await.map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    Ok(srv::json(StatusCode::OK, &json!({ "found": render_instances(hub, &found).await })))
}

#[derive(Deserialize)]
struct InviteRequest {
    instance: String,
}

/// `POST /api/discovery/invite`: let a machine found on the network know it may ask to join (for 30
/// minutes); it still needs approving.
async fn discovery_invite(hub: &Hub, req: Request) -> R {
    if !hub.cfg.discovery.join_requests {
        return Err(err(StatusCode::CONFLICT, "this server does not accept join requests ([discovery] join_requests = false)"));
    }
    let body: InviteRequest = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    if !hub.discovery.invites.invite(&body.instance, Instant::now()) {
        return Err(err(StatusCode::BAD_REQUEST, "not an instance id"));
    }
    let who = hub.discovery.table.get(&body.instance).map(|i| i.instance).unwrap_or_else(|| body.instance.clone());
    hub.store.audit("admin".into(), "discovery.invite".into(), who).await;
    hub.emit(&json!({ "type": "discovery" }));
    Ok(srv::json(StatusCode::OK, &json!({ "invited": true })))
}

/// `DELETE /api/discovery/invite/:instance`.
async fn discovery_uninvite(hub: &Hub, id: &str) -> Response {
    if hub.discovery.invites.revoke(id) {
        hub.store.audit("admin".into(), "discovery.uninvite".into(), id.to_owned()).await;
        hub.emit(&json!({ "type": "discovery" }));
    }
    srv::status(StatusCode::NO_CONTENT)
}

/// `GET /api/join/invited?instance=<id>` (open, rate-limited): whether this server invited that
/// machine, and its CA fingerprint (what it announces), for a machine waiting to be invited.
fn join_invited(hub: &Hub, req: &Request) -> Response {
    if !hub.cfg.discovery.join_requests {
        return srv::json(StatusCode::NOT_FOUND, &json!({ "error": "this server does not accept join requests" }));
    }
    let from = srv::remote_ip(req).unwrap_or(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
    let now = Instant::now();
    if let Err(wait) = hub.discovery.invites.allow(from, now) {
        let r = srv::json(StatusCode::TOO_MANY_REQUESTS, &json!({ "error": "too many questions; ask again later" }));
        return srv::with_header(r, header::RETRY_AFTER, &wait.as_secs().max(1).to_string());
    }
    let q = srv::query(req);
    let id = q.get("instance").map(String::as_str).unwrap_or_default();
    if !crate::discovery::invite::valid_id(id) {
        return srv::json(StatusCode::BAD_REQUEST, &json!({ "error": "instance must be an instance id" }));
    }
    srv::json(
        StatusCode::OK,
        &json!({ "invited": hub.discovery.invites.invited(id, now), "ca": crate::discovery::ca_fingerprint(&hub.pki.ca_der) }),
    )
}

/// The agent a discovered instance already is (or runs) for this server: an agent enrolled with our
/// CA, or a machine with one of our (persistent) agents on it.
fn joined_as(agents: &[crate::hub::AgentLive], i: &crate::discovery::Instance, ours: &str) -> Option<String> {
    let by_host = agents
        .iter()
        .filter(|a| !a.ephemeral)
        .find(|a| a.hostname.eq_ignore_ascii_case(&i.name) || a.name == crate::pki::sanitize_name(&i.name))
        .map(|a| a.name.clone());
    if i.role == "agent" && i.enrolled.as_deref() == Some(ours) {
        return Some(by_host.unwrap_or_else(|| crate::pki::sanitize_name(&i.name)));
    }
    by_host
}

/// This server's URL as `i` can reach it: the configured public URL when one was set, else our
/// address on the route to it.
fn our_url_for(hub: &Hub, i: &crate::discovery::Instance) -> String {
    let public = hub.cfg.server.public_url.trim_end_matches('/');
    let default = crate::config::Server::default().public_url;
    if public != default.trim_end_matches('/') && !crate::access::is_loopback(public) {
        return public.to_owned();
    }
    let port = hub.discovery.advert.lock().port;
    match i.address().and_then(crate::discovery::local_address_for) {
        Some(IpAddr::V4(ip)) => format!("http://{ip}:{port}"),
        Some(IpAddr::V6(ip)) => format!("http://[{ip}]:{port}"),
        None => public.to_owned(),
    }
}

fn refused(r: &crate::join::Refusal) -> Response {
    let code = StatusCode::from_u16(r.status()).unwrap_or(StatusCode::BAD_REQUEST);
    let resp = srv::json(code, &json!({ "error": r.message() }));
    match r {
        crate::join::Refusal::TooMany { retry_after, .. } => {
            srv::with_header(resp, header::RETRY_AFTER, &retry_after.as_secs().max(1).to_string())
        }
        _ => resp,
    }
}

/// `POST /api/join-requests` (open): a machine asks to join; an admin decides.
async fn create_join(hub: &Hub, req: Request) -> R {
    if !hub.cfg.discovery.join_requests {
        return Err(err(StatusCode::NOT_FOUND, "this server does not accept join requests"));
    }
    let from = srv::remote_ip(&req).unwrap_or(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
    let body: crate::join::NewRequest =
        srv::body_json(req.into_body(), 64 << 10).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    let (id, nonce) = match hub.joins.create(body, from, Instant::now()) {
        Ok(v) => v,
        Err(r) => return Ok(refused(&r)),
    };
    let name = hub.joins.get(&id, Instant::now(), &hub.pki.ca_der).map(|l| l.name).unwrap_or_default();
    tracing::info!(%id, agent = %name, %from, "join request received; waiting for an admin");
    hub.emit(&json!({ "type": "join_requests" }));
    // The machine's entry under discovery now has a pending request.
    hub.emit(&json!({ "type": "discovery" }));
    Ok(srv::json(
        StatusCode::CREATED,
        &crate::join::Created {
            id,
            nonce: hex::encode(nonce),
            ca_pem: hub.pki.ca_pem.clone(),
            expires_in: crate::join::REQUEST_TTL.as_secs(),
            server: crate::config::Config::hostname(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        },
    ))
}

/// `GET /api/join-requests/:id`: the machine polls with its secret (`Authorization: Bearer`); an
/// admin sees the request as listed.
async fn poll_join(hub: &Hub, id: &str, headers: &HeaderMap) -> R {
    let now = Instant::now();
    if authed(hub, headers) {
        return Ok(match hub.joins.get(id, now, &hub.pki.ca_der) {
            Ok(l) => srv::json(StatusCode::OK, &l),
            Err(r) => refused(&r),
        });
    }
    let Some(secret) = bearer(headers) else {
        return Err(err(StatusCode::UNAUTHORIZED, "the join secret is required (Authorization: Bearer <secret>)"));
    };
    match hub.joins.poll(id, secret.trim(), now, &hub.pki.ca_der) {
        Ok((polled, confirmed)) => {
            if confirmed {
                tracing::info!(%id, "join request confirmed by its machine");
                hub.emit(&json!({ "type": "join_requests" }));
                hub.emit(&json!({ "type": "discovery" }));
            }
            Ok(srv::json(StatusCode::OK, &polled))
        }
        Err(r) => {
            if r == crate::join::Refusal::WrongSecret {
                crate::rt::sleep(Duration::from_millis(300)).await;
            }
            Ok(refused(&r))
        }
    }
}

/// A name for an approved machine: the one it asked for, unless another machine's agent (or the
/// server's own local agent) has it, then `<name>-2`, `<name>-3`, …
async fn free_agent_name(hub: &Hub, wanted: &str, hostname: &str) -> String {
    let agents = hub.agent_list().await;
    let local = hub.cfg.local_agent.enabled.then(|| crate::pki::sanitize_name(&hub.cfg.local_agent_name()));
    let taken = |n: &str| {
        local.as_deref() == Some(n) || agents.iter().any(|a| a.name == n && !a.hostname.eq_ignore_ascii_case(hostname))
    };
    let mut name = wanted.to_owned();
    let mut n = 2u32;
    while taken(&name) && n < 1000 {
        let suffix = format!("-{n}");
        name = format!("{}{suffix}", crate::util::truncate(wanted, 63 - suffix.len()));
        n += 1;
    }
    name
}

/// `POST /api/join-requests/:id/approve`: sign the machine's CSR exactly as enrollment does.
async fn approve_join(hub: &Hub, id: &str, req: Request) -> R {
    let decision: crate::join::Decision = srv::body_json(req.into_body(), JSON_LIMIT).await.unwrap_or_default();
    let c = match hub.joins.candidate(id, Instant::now()) {
        Ok(c) => c,
        Err(r) => return Ok(refused(&r)),
    };
    // The discovered machine that asked, if it is listed (its invitation is used up).
    let asked = hub.joins.get(id, Instant::now(), &hub.pki.ca_der).ok().and_then(|listed| {
        let all = hub.discovery.table.snapshot();
        all.iter().find(|i| crate::discovery::invite::request_for(i, &all, std::slice::from_ref(&listed)).is_some()).map(|i| i.id.clone())
    });
    let wanted = decision.name.as_deref().map(crate::pki::sanitize_name).unwrap_or_else(|| c.name.clone());
    let name = free_agent_name(hub, &wanted, &c.hostname).await;
    // An outpost that asked instead of bringing an invite gets what an invited one gets.
    let outpost = c.labels.iter().any(|l| l == crate::proto::OUTPOST_LABEL);
    let labels = decision.labels.as_deref().map(crate::join::clean_labels).filter(|l| !l.is_empty()).unwrap_or(c.labels);
    let days = if outpost { crate::pki::OUTPOST_DAYS } else { 365 };
    let cert = hub.pki.sign_csr(&c.csr, &name, days).map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let quic = hub.cfg.server.quic_advertise.clone();
    let quic_port = hub.discovery.advert.lock().quic;
    let approval = crate::join::Approval {
        name: name.clone(),
        labels: labels.clone(),
        cert_pem: cert,
        ca_pem: hub.pki.ca_pem.clone(),
        quic,
        quic_port,
    };
    if let Err(r) = hub.joins.approve(id, approval, Instant::now()) {
        return Ok(refused(&r));
    }
    // A name forgotten earlier in this server's life is welcome again.
    hub.net.command(crate::net::Out::Unrevoke { name: name.clone() }).await;
    let what = if outpost { "outpost.join.approve" } else { "agent.join.approve" };
    hub.store.audit("admin".into(), what.into(), format!("{name} ({})", c.hostname)).await;
    tracing::info!(agent = %name, host = %c.hostname, outpost, "join request approved");
    if let Some(instance) = asked {
        hub.discovery.invites.revoke(&instance);
    }
    hub.emit(&json!({ "type": "join_requests" }));
    hub.emit(&json!({ "type": "discovery" }));
    Ok(srv::json(StatusCode::OK, &json!({ "id": id, "status": "approved", "name": name, "labels": labels })))
}

/// `POST /api/join-requests/:id/deny`.
async fn deny_join(hub: &Hub, id: &str) -> R {
    match hub.joins.deny(id, Instant::now()) {
        Ok(name) => {
            hub.store.audit("admin".into(), "agent.join.deny".into(), name.clone()).await;
            tracing::info!(agent = %name, "join request denied");
            hub.emit(&json!({ "type": "join_requests" }));
            hub.emit(&json!({ "type": "discovery" }));
            Ok(srv::json(StatusCode::OK, &json!({ "id": id, "status": "denied", "name": name })))
        }
        Err(r) => Ok(refused(&r)),
    }
}

/// The running binary, so new agents can be installed straight from the server.
async fn download_binary() -> R {
    let exe = std::env::current_exe().map_err(anyhow::Error::from)?;
    let body = crate::rt::file_source(exe)?;
    Ok(srv::response(StatusCode::OK, "application/octet-stream", srv::channel_body(body)))
}

/// The binary for `target`: this server's own when it was built for that target, one uploaded
/// or fetched here (Settings → Updates), otherwise a redirect to the release build of the same
/// version on GitHub.
async fn download_for(hub: &Hub, target: &str) -> R {
    if target == TARGET {
        return download_binary().await;
    }
    if let Some((path, _)) = crate::updates::stored(hub, target) {
        let body = crate::rt::file_source(path)?;
        return Ok(srv::response(StatusCode::OK, "application/octet-stream", srv::channel_body(body)));
    }
    if !RELEASE_TARGETS.contains(&target) {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("no kiln build for {target}; available: {}, {TARGET}", RELEASE_TARGETS.join(", ")),
        ));
    }
    let r = srv::with_header(srv::status(StatusCode::FOUND), header::LOCATION, &release_url(target));
    Ok(srv::with_header(r, header::CACHE_CONTROL, "no-store"))
}

/// Build `build` for `target`: what an upgrade names, so a machine gets exactly the binary it
/// was told the SHA-256 of (kiln may hand out more than one for a platform).
async fn download_build(hub: &Hub, target: &str, build: &str) -> R {
    let Some(b) = crate::updates::binaries(hub).into_iter().find(|b| b.target == target && b.build == build) else {
        return Err(err(StatusCode::NOT_FOUND, format!("kiln has no build {build} for {target}")));
    };
    if b.origin == "server" {
        return download_binary().await;
    }
    let (path, _) = crate::updates::stored(hub, target).filter(|(_, m)| m.build == build).ok_or_else(|| err(StatusCode::NOT_FOUND, format!("kiln has no build {build} for {target}")))?;
    let body = crate::rt::file_source(path)?;
    Ok(srv::response(StatusCode::OK, "application/octet-stream", srv::channel_body(body)))
}

fn build_token_ok(hub: &Hub, headers: &HeaderMap, job: &str) -> bool {
    bearer(headers).is_some_and(|t| crate::util::ct_eq(t.as_bytes(), crate::fleet::token(hub, job).as_bytes()))
}

/// `GET /api/updates/jobs/{id}/source`: the source of the commit a machine builds itself
/// (`crate::fleet`), as tar.
async fn build_source(hub: &Hub, id: &str, headers: &HeaderMap) -> R {
    if !build_token_ok(hub, headers, id) {
        return Err(err(StatusCode::UNAUTHORIZED, "bad build token"));
    }
    let stdout = crate::fleet::source(hub, id).await.map_err(|e| err(StatusCode::NOT_FOUND, format!("{e:#}")))?;
    Ok(srv::response(StatusCode::OK, "application/x-tar", srv::channel_body(stdout)))
}

/// `POST /api/updates/jobs/{id}/binary?sha256=<hex>`: what a machine built of itself; the binary
/// as the body when it is kept for the machine's platform, else nothing.
async fn build_binary(hub: &Arc<Hub>, id: &str, q: &HashMap<String, String>, req: Request) -> R {
    if !build_token_ok(hub, req.headers(), id) {
        return Err(err(StatusCode::UNAUTHORIZED, "bad build token"));
    }
    let sha256 = q.get("sha256").cloned().unwrap_or_default();
    let bytes = srv::body_bytes(req.into_body(), crate::updates::MAX_BINARY).await.map_err(|e| err(StatusCode::PAYLOAD_TOO_LARGE, format!("{e:#}")))?;
    crate::fleet::on_built(hub, id, &sha256, Some(bytes.to_vec())).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

// ------------------------------------------------------------------ updates (Settings → Updates)

/// `GET /api/updates`: this server and how it is upgraded, every machine's runner, the latest
/// upgrades and the binaries kiln can hand out. `POST /api/updates/check` asks GitHub (and, in
/// Kubernetes, the image on the node) again; an `updates` event follows when that is done.
async fn updates_overview(hub: &Arc<Hub>, force: bool) -> Response {
    let runners = crate::updates::runners(hub).await;
    let mut targets: Vec<&str> = RELEASE_TARGETS.to_vec();
    if !targets.contains(&TARGET) {
        targets.insert(0, TARGET);
    }
    let jobs: Vec<crate::updates::Job> = hub.updates.jobs().into_iter().take(12).collect();
    srv::json(
        StatusCode::OK,
        &json!({
            "server": crate::updates::server(hub, force),
            "runners": runners,
            "jobs": jobs,
            "binaries": crate::updates::binaries(hub),
            "targets": targets,
            "build": release_build(hub).await,
            "newest": hub.updates.fleet.target(),
            "follow": hub.cfg.updates.follow,
        }),
    )
}

/// The pipeline that builds kiln for every platform (`[updates]`), and its latest run.
async fn release_build(hub: &Hub) -> serde_json::Value {
    let u = &hub.cfg.updates;
    let repo = hub.store.repos().await.unwrap_or_default().into_iter().find(|r| r.name == u.release_repo);
    let Some(repo) = repo else {
        return json!({ "repo": u.release_repo, "pipeline": u.release_pipeline, "error": format!("kiln doesn't build {} (no repository of that name)", u.release_repo) });
    };
    let last = hub.store.runs(Some(repo.id.clone()), 100, None).await.unwrap_or_default().into_iter().find(|r| r.pipeline == u.release_pipeline);
    // The newest kiln (`crate::fleet`): the server's commit, or a newer one a runner runs.
    let newest = hub.updates.fleet.target().map_or_else(|| COMMIT.to_owned(), |t| t.commit);
    let s = crate::updates::in_step(hub, &newest);
    json!({
        "repo": u.release_repo,
        "pipeline": u.release_pipeline,
        "branch": u.release_branch,
        "last": last,
        "commit": s.commit,
        "in_step": s.in_step,
        "behind": s.behind,
        "keeps_in_step": u.in_step,
    })
}

/// `POST /api/updates/build`: run the pipeline that builds kiln for every platform; when it
/// succeeds, every platform's runners can be upgraded from here.
async fn build_every_platform(hub: &Hub) -> R {
    let u = &hub.cfg.updates;
    let repo = hub.store.repos().await?.into_iter().find(|r| r.name == u.release_repo).ok_or_else(|| err(StatusCode::NOT_FOUND, format!("kiln doesn't build {}", u.release_repo)))?;
    // The newest kiln (`crate::fleet`), when kiln knows it: only that build is what runners take.
    let newest = hub.updates.fleet.target().map_or_else(|| COMMIT.to_owned(), |t| t.commit);
    let run = if newest.is_empty() {
        let runs = crate::gitops::manual(hub, &repo, Some(u.release_branch.clone()), Some(u.release_pipeline.clone()))
            .await
            .map_err(|e| err(StatusCode::CONFLICT, format!("{e:#}")))?;
        runs.into_iter().next().ok_or_else(|| err(StatusCode::CONFLICT, format!("{} has no pipeline called {}", u.release_repo, u.release_pipeline)))?
    } else {
        crate::updates::build_commit(hub, &repo, &newest).await.map_err(|e| err(StatusCode::CONFLICT, format!("{e:#}")))?
    };
    hub.store.audit("admin".into(), "upgrade.build".into(), format!("{} {} #{}", u.release_repo, u.release_pipeline, run.number)).await;
    Ok(srv::json(StatusCode::ACCEPTED, &run))
}

/// `POST /api/updates/runners/{name}`: upgrade that machine's runner (nothing it runs stops).
async fn upgrade_runner(hub: &Arc<Hub>, name: &str) -> R {
    match crate::updates::upgrade_runner(hub, name).await {
        Ok(job) => Ok(srv::json(StatusCode::ACCEPTED, &job)),
        Err(e) => Err(err(StatusCode::CONFLICT, format!("{e:#}"))),
    }
}

#[derive(Deserialize)]
struct RunnerPolicy {
    policy: crate::updates::Policy,
}

/// `PUT /api/updates/runners/{name}` `{"policy": "auto" | "notify" | "off"}`: what that machine's
/// runner does with a newer kiln (Notify unless chosen otherwise).
async fn set_runner_policy(hub: &Arc<Hub>, name: &str, req: Request) -> R {
    let b: RunnerPolicy = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    apply_policy(hub, &[name.to_owned()], b.policy).await
}

#[derive(Deserialize)]
struct Policies {
    policy: crate::updates::Policy,
    /// These runners; or every one, with `all`.
    #[serde(default)]
    runners: Vec<String>,
    #[serde(default)]
    all: bool,
}

/// `PUT /api/updates/policy` `{"policy": "off", "runners": ["a", "b"]}` (or `"all": true`): the same
/// update policy for any number of runners at once.
async fn set_policies(hub: &Arc<Hub>, req: Request) -> R {
    let b: Policies = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let names = if b.all { crate::updates::runners(hub).await.into_iter().map(|r| r.name).collect() } else { b.runners };
    if names.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "name the runners, or say all"));
    }
    apply_policy(hub, &names, b.policy).await
}

async fn apply_policy(hub: &Arc<Hub>, names: &[String], policy: crate::updates::Policy) -> R {
    for name in names {
        crate::updates::set_policy(hub, name, policy).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    }
    hub.store.audit("admin".into(), "upgrade.policy".into(), format!("updates {} for {}", policy.as_str(), names.join(", "))).await;
    hub.emit(&json!({ "type": "updates" }));
    // Its gauges at once, and an automatic one brought up to date.
    hub.updates.fleet.wake();
    Ok(srv::status(StatusCode::NO_CONTENT))
}

/// `POST /api/updates/runners`: upgrade every connected runner that isn't up to date.
async fn upgrade_all_runners(hub: &Arc<Hub>) -> Response {
    let mut jobs = Vec::new();
    let mut errors = Vec::new();
    for r in crate::updates::upgrade_all(hub).await {
        match r {
            Ok(j) => jobs.push(j),
            Err(e) => errors.push(e),
        }
    }
    srv::json(StatusCode::ACCEPTED, &json!({ "jobs": jobs, "errors": errors }))
}

#[derive(Deserialize)]
struct ServerUpgrade {
    action: String,
}

/// `POST /api/updates/server` `{action: "restart" | "replace"}`.
async fn upgrade_server(hub: &Arc<Hub>, req: Request) -> R {
    let b: ServerUpgrade = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    match crate::updates::upgrade_server(hub, &b.action).await {
        Ok(job) => Ok(srv::json(StatusCode::ACCEPTED, &job)),
        Err(e) => Err(err(StatusCode::CONFLICT, format!("{e:#}"))),
    }
}

/// `POST /api/updates/binaries?target=<platform>`: a kiln binary for that platform (the raw
/// file as the body), handed to its machines and, for this server's platform, installable here.
async fn upload_binary(hub: &Arc<Hub>, q: &HashMap<String, String>, req: Request) -> R {
    let target = q.get("target").cloned().unwrap_or_default();
    if !crate::updates::known_target(&target) {
        return Err(err(StatusCode::BAD_REQUEST, format!("unknown platform {target:?}: one of {TARGET}, {}", RELEASE_TARGETS.join(", "))));
    }
    let bytes = srv::body_bytes(req.into_body(), crate::updates::MAX_BINARY).await.map_err(|e| err(StatusCode::PAYLOAD_TOO_LARGE, format!("{e:#}")))?;
    let (h, t) = (Arc::clone(hub), target.clone());
    let b = crate::rt::blocking(move || crate::updates::store(&h, &t, &bytes, "upload"))
        .await
        .and_then(|r| r)
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    hub.store.audit("admin".into(), "upgrade.binary".into(), format!("uploaded build {} for {target}", b.build)).await;
    hub.emit(&json!({ "type": "updates" }));
    Ok(srv::json(StatusCode::OK, &b))
}

/// `DELETE /api/updates/binaries/{target}`.
async fn delete_binary(hub: &Hub, target: &str) -> R {
    if !crate::updates::unstore(hub, target) {
        return Err(err(StatusCode::NOT_FOUND, format!("no binary is kept for {target}")));
    }
    hub.store.audit("admin".into(), "upgrade.binary".into(), format!("removed the binary for {target}")).await;
    hub.emit(&json!({ "type": "updates" }));
    Ok(srv::status(StatusCode::NO_CONTENT))
}

/// An install script with this server's URL and build target filled in.
fn render_script(hub: &Hub, template: &str) -> String {
    let url = hub.cfg.server.public_url.trim_end_matches('/').to_owned();
    template.replace("@@KILN_SERVER@@", &url).replace("@@KILN_TARGET@@", TARGET)
}

/// Linux and macOS agents: `curl -fsSL <server>/install.sh | sh -s -- --token ...`.
fn install_script(hub: &Hub) -> Response {
    let script = render_script(hub, include_str!("../assets/install-agent.sh"));
    srv::response(StatusCode::OK, "text/x-shellscript", srv::full(script))
}

/// Windows agents: `& ([scriptblock]::Create((irm <server>/install.ps1))) -Token ...`.
fn install_powershell(hub: &Hub) -> Response {
    let script = render_script(hub, include_str!("../assets/install-agent.ps1"));
    srv::response(StatusCode::OK, "text/plain; charset=utf-8", srv::full(script))
}

// ------------------------------------------------------------------ webhooks

async fn github_hook(hub: &Arc<Hub>, req: Request) -> R {
    let sig = srv::header_str(&req, &header::HeaderName::from_static("x-hub-signature-256")).unwrap_or("").to_owned();
    let event = srv::header_str(&req, &header::HeaderName::from_static("x-github-event")).unwrap_or("").to_owned();
    let delivery = srv::header_str(&req, &header::HeaderName::from_static("x-github-delivery")).map(str::to_owned);
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
    // Without the CI engine a webhook only hints; the poll decides what changed (GitOps: state
    // comes from git). With it, GitHub's payload reaches the workflows (docs/ci.md W03-R07).
    let hub = Arc::clone(hub);
    crate::rt::spawn(async move {
        if let Err(e) = crate::ci::hooks::github(&hub, &event, delivery, &body).await {
            tracing::warn!(event = %event, error = %format!("{e:#}"), "webhook");
            hub.gitops_kick.notify();
        }
    });
    Ok(srv::status(StatusCode::ACCEPTED))
}

// ------------------------------------------------------------------ agent tunnel

/// `GET /api/agents/tunnel`: a runner's QUIC datagrams through a WebSocket, for runners whose
/// UDP to the QUIC port is dropped (see `crate::tunnel`). Open: the QUIC mutual TLS inside it
/// authenticates both ends.
fn agent_tunnel(hub: &Arc<Hub>, req: Request) -> R {
    let bind: std::net::SocketAddr = hub
        .cfg
        .server
        .quic_addr
        .parse()
        .map_err(|_| err(StatusCode::SERVICE_UNAVAILABLE, "the QUIC listener address is not an address"))?;
    crate::tunnel::serve(&hub.tunnels, crate::tunnel::listener_for(bind), req).map_err(|(status, why)| err(status, why))
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

#[derive(Deserialize)]
struct NewTerminal {
    /// "local" or "agent:<machine>".
    target: String,
    #[serde(default)]
    cmd: Option<String>,
    #[serde(default)]
    cols: Option<u16>,
    #[serde(default)]
    rows: Option<u16>,
}

/// Start a terminal session without attaching to it (an AI agent launched from the Agents page);
/// any device then attaches by its id.
#[derive(serde::Deserialize)]
struct TerminalName {
    #[serde(default)]
    name: String,
}

/// Names a terminal session (`{"name": ""}` takes its name away).
async fn rename_terminal(hub: &Arc<Hub>, id: &str, req: Request) -> R {
    let t: TerminalName = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    let (h, sid) = (Arc::clone(hub), id.to_owned());
    if !crate::rt::blocking(move || h.terminals.rename(&sid, &t.name)).await.unwrap_or(false) {
        return Err(err(StatusCode::NOT_FOUND, "no such terminal session"));
    }
    hub.emit_terminals();
    Ok(srv::status(StatusCode::NO_CONTENT))
}

async fn create_terminal(hub: &Arc<Hub>, req: Request) -> R {
    if !hub.cfg.terminal.enabled {
        return Err(err(StatusCode::FORBIDDEN, "terminal disabled in config"));
    }
    let t: NewTerminal = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    if hub.terminals.live() >= hub.cfg.terminal.max_sessions {
        return Err(err(StatusCode::TOO_MANY_REQUESTS, "too many terminal sessions"));
    }
    let (cols, rows) = (t.cols.unwrap_or(120), t.rows.unwrap_or(40));
    let session = match t.target.strip_prefix("agent:") {
        Some(machine) => {
            if !hub.agents.lock().get(machine).is_some_and(|a| a.connected) {
                return Err(err(StatusCode::CONFLICT, format!("{machine} is not connected")));
            }
            hub.store.audit("admin".into(), "terminal.open".into(), t.target.clone()).await;
            crate::termsess::open_agent(hub, machine.to_owned(), t.cmd, cols, rows)
        }
        None if t.target == "local" => {
            hub.store.audit("admin".into(), "terminal.open".into(), t.target.clone()).await;
            crate::termsess::open_local(hub, t.cmd, cols, rows).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
        }
        None => return Err(err(StatusCode::BAD_REQUEST, "target is \"local\" or \"agent:<machine>\"")),
    };
    Ok(srv::json(StatusCode::CREATED, &session.info()))
}

async fn terminal_ws(hub: Arc<Hub>, q: HashMap<String, String>, req: Request) -> R {
    if !hub.cfg.terminal.enabled {
        return Err(err(StatusCode::FORBIDDEN, "terminal disabled in config"));
    }
    let existing = q.get("session").cloned();
    if existing.is_none() && hub.terminals.live() >= hub.cfg.terminal.max_sessions {
        return Err(err(StatusCode::TOO_MANY_REQUESTS, "too many terminal sessions"));
    }
    let (resp, upgrade) = ws::accept(req).map_err(|r| ApiError(r.status, r.reason.into()))?;
    let target = q.get("target").cloned().unwrap_or_else(|| "local".into());
    let cols = q.get("cols").and_then(|v| v.parse().ok()).unwrap_or(120);
    let rows = q.get("rows").and_then(|v| v.parse().ok()).unwrap_or(40);
    let cmd = q.get("cmd").cloned();
    crate::rt::spawn(async move {
        let Ok(socket) = upgrade.await else { return };
        if let Err(e) = terminal_attach(hub, existing, target, (cols, rows), cmd, "admin".into(), socket).await {
            tracing::debug!(error = %format!("{e:#}"), "terminal websocket ended");
        }
    });
    Ok(resp)
}

/// `GET /api/ci/legs/:id/terminal` (WebSocket): a terminal inside a running or kept CI leg
/// (docs/ci.md W17-R01), on the terminal protocol of `/api/terminal/ws`; `?session=` attaches
/// again to one of that leg's.
async fn leg_terminal_ws(hub: Arc<Hub>, id: &str, q: HashMap<String, String>, caller: &crate::authz::Caller, req: Request) -> R {
    let leg = id.to_owned();
    let row: Option<(Option<String>, Option<String>, String, Option<i64>)> = hub
        .store
        .call(move |c| {
            use rusqlite::OptionalExtension as _;
            Ok(c.query_row("SELECT agent, executor, status, kept_until FROM jobs WHERE id = ?1 AND job_id IS NOT NULL", [&leg], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?)
        })
        .await?;
    let Some((agent, executor, status, kept_until)) = row else { return Err(err(StatusCode::NOT_FOUND, "no such leg of the CI engine")) };
    let kept = kept_until.is_some_and(|t| t > crate::util::now_unix());
    let Some(agent) = agent.filter(|_| status == "running" || kept) else {
        return Err(err(StatusCode::CONFLICT, "the leg isn't running or kept: a terminal opens into a running leg, or a failed one kept with keep-on-failure"));
    };
    // On the host executor a terminal is a shell on the runner itself.
    if executor.as_deref() == Some("host") && !caller.can("runners.terminal", &crate::authz::Scope::Runner { runner: agent.clone() }) {
        return Err(err(StatusCode::FORBIDDEN, format!("{} may not open a shell on runner {agent}: a host leg's terminal needs runners.terminal there", caller.name)));
    }
    let existing = q.get("session").cloned();
    if let Some(s) = &existing
        && hub.terminals.get(s).is_some_and(|t| t.target != format!("leg:{id}"))
    {
        return Err(err(StatusCode::FORBIDDEN, "that terminal isn't this leg's"));
    }
    if existing.is_none() && hub.terminals.live() >= hub.cfg.terminal.max_sessions {
        return Err(err(StatusCode::TOO_MANY_REQUESTS, "too many terminal sessions"));
    }
    let (resp, upgrade) = ws::accept(req).map_err(|r| ApiError(r.status, r.reason.into()))?;
    let cols = q.get("cols").and_then(|v| v.parse().ok()).unwrap_or(120);
    let rows = q.get("rows").and_then(|v| v.parse().ok()).unwrap_or(40);
    let (target, actor) = (format!("leg:{id}@{agent}"), caller.name.clone());
    crate::rt::spawn(async move {
        let Ok(socket) = upgrade.await else { return };
        if let Err(e) = terminal_attach(hub, existing, target, (cols, rows), None, actor, socket).await {
            tracing::debug!(error = %format!("{e:#}"), "leg terminal websocket ended");
        }
    });
    Ok(resp)
}

/// One browser on one session: identity, replay, then live output; input both ways. Closing the
/// socket only detaches — the shell keeps running for the next attach.
async fn terminal_attach(hub: Arc<Hub>, existing: Option<String>, target: String, (cols, rows): (u16, u16), cmd: Option<String>, actor: String, socket: ws::WebSocket) -> anyhow::Result<()> {
    use crate::termsess::{Frame, Input};
    use futures::FutureExt as _;
    let (mut tx, mut rx) = socket.split();
    let session = match existing {
        Some(id) => match hub.terminals.get(&id) {
            Some(s) => s,
            None => {
                // Not (yet) known: e.g. its machine is reconnecting. The client retries.
                tx.send(ws::text(json!({"type": "missing", "session": id}).to_string())).await?;
                return Ok(());
            }
        },
        None => {
            hub.store.audit(actor, "terminal.open".into(), target.clone()).await;
            if let Some((job, machine)) = target.strip_prefix("leg:").and_then(|t| t.split_once('@')) {
                crate::termsess::open_leg(&hub, machine.to_owned(), job, cols, rows)
            } else if let Some(machine) = target.strip_prefix("agent:") {
                crate::termsess::open_agent(&hub, machine.to_owned(), cmd, cols, rows)
            } else {
                match crate::termsess::open_local(&hub, cmd, cols, rows) {
                    Ok(s) => s,
                    Err(e) => {
                        tx.send(ws::text(json!({"type": "error", "message": format!("{e:#}")}).to_string())).await?;
                        return Ok(());
                    }
                }
            }
        }
    };
    let attachment = session.attach();
    let mut live = attachment.live;
    let result: anyhow::Result<()> = async {
        // Read after subscribing: a resize from here on also arrives as a live size frame.
        tx.send(ws::text(json!({"type": "session", "session": session.info()}).to_string())).await?;
        if !attachment.replay.is_empty() {
            tx.send(ws::binary(attachment.replay)).await?;
        }
        if let Some((code, error)) = attachment.ended {
            let msg = match error {
                Some(e) => json!({"type": "error", "message": e}),
                None => json!({"type": "exit", "code": code}),
            };
            tx.send(ws::text(msg.to_string())).await.ok();
            return Ok(());
        }
        loop {
            futures::select_biased! {
                f = live.recv().fuse() => match f {
                    Ok(Frame::Data(d)) => tx.send(ws::binary(d)).await?,
                    Ok(Frame::Size { cols, rows }) => tx.send(ws::text(json!({"type": "size", "cols": cols, "rows": rows}).to_string())).await?,
                    Ok(Frame::Exit { error: Some(e), .. }) if e == "__reconnect__" => {
                        tx.send(ws::text(json!({"type": "reconnect"}).to_string())).await.ok();
                        return Ok(());
                    }
                    Ok(Frame::Exit { code, error }) => {
                        let msg = match error {
                            Some(e) => json!({"type": "error", "message": e}),
                            None => json!({"type": "exit", "code": code}),
                        };
                        tx.send(ws::text(msg.to_string())).await.ok();
                        return Ok(());
                    }
                    Err(async_broadcast::RecvError::Overflowed(_)) => {
                        // This viewer fell behind; repaint it from the session's state.
                        let a = session.attach();
                        session.detach();
                        tx.send(ws::binary(a.replay)).await?;
                    }
                    Err(async_broadcast::RecvError::Closed) => return Ok(()),
                },
                m = rx.next().fuse() => match m {
                    Some(Ok(Message::Binary(b))) => session.send(Input::Data(b.to_vec())),
                    Some(Ok(Message::Text(t))) => match serde_json::from_str::<TermCtl>(t.as_str()) {
                        Ok(TermCtl::Resize { cols, rows }) => session.send(Input::Resize { cols, rows }),
                        Ok(TermCtl::Input { data }) => session.send(Input::Data(data.into_bytes())),
                        Ok(TermCtl::Ping) | Err(_) => {}
                    },
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return Ok(()),
                    _ => {}
                },
            }
        }
    }
    .await;
    session.detach();
    hub.emit_terminals();
    result
}

// ------------------------------------------------------------------ agent sessions (kiln-native)

/// The harnesses each machine offers; `?target=` narrows it to one ("local", "agent:<name>").
fn harness_kinds(hub: &Hub, q: &HashMap<String, String>) -> Response {
    let all = hub.harness.kinds();
    match q.get("target") {
        Some(t) => srv::json(StatusCode::OK, &all.get(t).cloned().unwrap_or_default()),
        None => srv::json(StatusCode::OK, &all),
    }
}

/// A machine's past conversations to continue (`?target=local|agent:<name>`, `kind=claude|codex`,
/// `limit=`): newest first; resume one with `spec.resume = id`.
async fn harness_history(hub: &Hub, q: &HashMap<String, String>) -> R {
    let target = q.get("target").map_or("local", String::as_str);
    if target != "local" && target.strip_prefix("agent:").is_none_or(str::is_empty) {
        return Err(err(StatusCode::BAD_REQUEST, "target is \"local\" or \"agent:<name>\""));
    }
    let kind = q.get("kind").filter(|k| !k.is_empty()).cloned();
    if kind.as_deref().is_some_and(|k| k != "claude" && k != "codex") {
        return Err(err(StatusCode::BAD_REQUEST, "kind is \"claude\" or \"codex\""));
    }
    let limit = q.get("limit").and_then(|l| l.parse::<usize>().ok()).unwrap_or(50);
    match hub.harness.history(hub, target, kind, limit).await {
        Ok(Some(v)) => Ok(srv::json(StatusCode::OK, &v)),
        Ok(None) => Err(err(StatusCode::NOT_FOUND, "that machine keeps no sessions (an ephemeral runner)")),
        Err(e) => Err(err(StatusCode::BAD_GATEWAY, format!("{e:#}"))),
    }
}

/// `GET /api/harness/sessions/:id/events?before=&limit=`: a session's history beyond what its
/// timeline holds, from the server's conversation store (docs/conversations.md): `{events}`, oldest
/// first, the newest `limit` (100, at most 1000) before event number `before`.
async fn conversation_events(hub: &Hub, id: &str, q: &HashMap<String, String>) -> R {
    let Some(store) = hub.harness.convo.clone() else { return Err(err(StatusCode::NOT_FOUND, "this server keeps no conversations (KILN_CONVERSATIONS is off)")) };
    let before = q.get("before").and_then(|b| b.parse::<u64>().ok());
    let limit = q.get("limit").and_then(|l| l.parse::<usize>().ok()).unwrap_or(100).clamp(1, 1000);
    let id = id.to_owned();
    let rows = crate::rt::blocking(move || store.events(&id, before, limit)).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let rows = rows.map_err(|e| err(StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")))?;
    let events: Vec<serde_json::Value> = rows.iter().filter_map(|r| serde_json::from_str(&r.body).ok()).collect();
    Ok(srv::json(StatusCode::OK, &json!({ "events": events })))
}

/// `GET /api/conversations/search?q=&agent=&kind=&tool=&status=&since=&until=&limit=`: events across
/// every agent's conversation, newest first, from the server's store: `{hits: [{agent, name, machine,
/// event}]}`.
async fn conversation_search(hub: &Hub, q: &HashMap<String, String>) -> R {
    let Some(store) = hub.harness.convo.clone() else { return Err(err(StatusCode::NOT_FOUND, "this server keeps no conversations (KILN_CONVERSATIONS is off)")) };
    let text = |k: &str| q.get(k).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
    let num = |k: &str| q.get(k).and_then(|v| v.parse::<i64>().ok());
    let query = crate::convo::store::Query {
        q: text("q").unwrap_or_default(),
        session: text("agent"),
        kind: text("kind"),
        tool: text("tool"),
        status: text("status"),
        since: num("since"),
        until: num("until"),
        limit: q.get("limit").and_then(|v| v.parse::<usize>().ok()),
    };
    let rows = crate::rt::blocking(move || store.search(query)).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let rows = rows.map_err(|e| err(StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")))?;
    let infos: HashMap<String, crate::harness::session::Info> = hub.harness.list().into_iter().map(|i| (i.id.clone(), i)).collect();
    let hits: Vec<serde_json::Value> = rows
        .iter()
        .filter_map(|r| {
            let event: serde_json::Value = serde_json::from_str(&r.body).ok()?;
            let info = infos.get(&r.session);
            Some(json!({ "agent": r.session, "name": info.map(|i| i.name.clone()), "machine": info.map(|i| i.target.clone()), "event": event }))
        })
        .collect();
    Ok(srv::json(StatusCode::OK, &json!({ "hits": hits })))
}

fn goals_err(e: crate::goals::Error) -> ApiError {
    match e {
        crate::goals::Error::Bad(m) => err(StatusCode::BAD_REQUEST, m),
        crate::goals::Error::NoAgent => err(StatusCode::NOT_FOUND, "no such agent"),
        crate::goals::Error::Other(e) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

#[derive(Deserialize)]
struct GoalsSet {
    goals: Vec<crate::goals::Given>,
}

/// An agent's goals not yet started, as they should be (`crate::goals::set`).
async fn goals_set(hub: &Hub, id: &str, req: Request) -> R {
    let b: GoalsSet = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let s = crate::goals::set(hub, id, b.goals).await.map_err(goals_err)?;
    hub.store.audit("admin".into(), "goals.set".into(), id.to_owned()).await;
    Ok(srv::json(StatusCode::OK, &s))
}

#[derive(Deserialize)]
struct GoalsAdd {
    goals: Vec<String>,
    #[serde(default)]
    start: bool,
}

/// Goals lined up after the agent's others (`crate::goals::add`).
async fn goals_add(hub: &Hub, id: &str, req: Request) -> R {
    let b: GoalsAdd = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let s = crate::goals::add(hub, id, b.goals, b.start).await.map_err(goals_err)?;
    hub.store.audit("admin".into(), "goals.add".into(), id.to_owned()).await;
    Ok(srv::json(StatusCode::OK, &s))
}

/// Start, pause, skip or clear an agent's goal stack.
async fn goals_control(hub: &Hub, id: &str, action: &str) -> R {
    let a: crate::goals::Action = serde_json::from_value(json!(action)).map_err(|_| err(StatusCode::NOT_FOUND, "not found"))?;
    let s = crate::goals::control(hub, id, a).await.map_err(goals_err)?;
    hub.store.audit("admin".into(), "goals.control".into(), format!("{id}: {action}")).await;
    Ok(srv::json(StatusCode::OK, &s))
}

/// A terminal beside an agent: a shell in its folder, or its harness's own screen
/// (`crate::harness::screen`).
async fn harness_terminal(hub: &Arc<Hub>, id: &str, req: Request) -> R {
    if !hub.cfg.terminal.enabled {
        return Err(err(StatusCode::FORBIDDEN, "terminal disabled in config"));
    }
    let o: crate::harness::screen::Open = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    if hub.harness.list().iter().all(|i| i.id != id) {
        return Err(err(StatusCode::NOT_FOUND, "no such agent"));
    }
    let b = crate::harness::screen::open(hub, id, o).await.map_err(|e| err(StatusCode::CONFLICT, format!("{e:#}")))?;
    Ok(srv::json(StatusCode::CREATED, &b))
}

#[derive(Deserialize)]
struct HarnessOpen {
    target: String,
    spec: crate::harness::session::Spec,
    #[serde(default)]
    count: Option<usize>,
}

async fn harness_open(hub: &Hub, req: Request) -> R {
    let opener = caller_of(hub, req.headers()).await.and_then(|c| c.opener());
    let mut body: HarnessOpen = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    // Ending a process found running is asked for with a take-over, never with a spec.
    body.spec.takeover_stop = false;
    // Agents run programs as the machine's user, like terminals: the same switch governs both.
    if body.target == "local" && !hub.cfg.terminal.enabled {
        return Err(err(StatusCode::FORBIDDEN, "terminals (and so agents) are disabled on this server"));
    }
    match crate::harness::kinds::Kind::get(&body.spec.kind) {
        None => return Err(err(StatusCode::BAD_REQUEST, format!("unknown harness {:?}", body.spec.kind))),
        Some(k) if !k.structured() => {
            return Err(err(StatusCode::BAD_REQUEST, format!("{} has no structured mode: open a terminal and run `{}`", k.name, k.program)));
        }
        Some(_) => {}
    }
    hub.store.audit("admin".into(), "agent.open".into(), format!("{} on {}", body.spec.kind, body.target)).await;
    let infos = hub.harness.open(hub, &body.target, body.spec, body.count.unwrap_or(1)).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    // Whom each acts for when its MCP server asks kiln for something (`crate::mcp::runner_ask`).
    if let Some(o) = &opener {
        for i in &infos {
            let _ = hub.store.kv_set(crate::mcp::opener_key(&i.id), o.clone()).await;
        }
    }
    Ok(srv::json(StatusCode::OK, &infos))
}

/// Every agent of a machine that's gone goes on elsewhere, from the server's copy of its
/// conversation (`harness::relocate::recover`): `{"to"?}` puts them all on one machine.
async fn harness_recover(hub: &Arc<Hub>, machine: &str, req: Request) -> R {
    #[derive(serde::Deserialize, Default)]
    struct Body {
        #[serde(default)]
        to: Option<String>,
    }
    let b: Body = srv::body_json(req.into_body(), JSON_LIMIT).await.unwrap_or_default();
    let moved = crate::harness::relocate::recover(hub, machine, b.to.as_deref()).await.map_err(|e| err(StatusCode::CONFLICT, e))?;
    let ok = moved.iter().filter(|m| m.get("error").is_none()).count();
    hub.store.audit("admin".into(), "agent.recover".into(), format!("{ok} of {} agents of {machine} go on elsewhere", moved.len())).await;
    Ok(srv::json(StatusCode::OK, &serde_json::json!({ "moved": moved })))
}

/// One thing to many agents at once: a prompt or slash command, a command, or answers to every
/// tool call waiting on you (see `crate::harness::broadcast`). Each agent's outcome comes back.
async fn harness_broadcast(hub: &Arc<Hub>, req: Request) -> R {
    let b: crate::harness::broadcast::Broadcast = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let report = crate::harness::broadcast::run(hub, &b).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let what = b.text.as_deref().map_or_else(|| b.approvals.as_deref().map_or_else(|| "a command".to_owned(), |a| format!("{a} what waits")), |t| t.chars().take(80).collect());
    hub.store.audit("admin".into(), "agent.broadcast".into(), format!("{} to {} agents ({} sent, {} skipped, {} failed): {what}", report.id, report.results.len(), report.sent, report.skipped, report.failed)).await;
    Ok(srv::json(StatusCode::OK, &report))
}

/// Who a broadcast to `to` would reach (the same picking as the broadcast itself), the machines
/// they are on, and what they understand: their catalogs (slash commands, modes, models, efforts)
/// merged, each entry with `count`, how many of them have it.
async fn harness_broadcast_preview(hub: &Arc<Hub>, req: Request) -> R {
    #[derive(serde::Deserialize)]
    struct Body {
        to: crate::harness::broadcast::To,
    }
    let b: Body = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let picked = crate::harness::broadcast::pick(&hub.harness.list(), &b.to);
    let mut machines: Vec<String> = Vec::new();
    // (field in each agent's meta, the key naming an entry, the entries seen with their counts)
    let mut merged: [(&str, &str, Vec<(serde_json::Value, usize)>); 4] =
        [("commands", "name", Vec::new()), ("modes", "id", Vec::new()), ("models", "id", Vec::new()), ("efforts", "id", Vec::new())];
    for info in &picked {
        let machine = info.target.strip_prefix("agent:").unwrap_or(&info.target).to_owned();
        if !machines.contains(&machine) {
            machines.push(machine);
        }
        let Some(meta) = hub.harness.meta(&info.id) else { continue };
        for (field, key, seen) in &mut merged {
            for entry in meta.get(*field).and_then(serde_json::Value::as_array).into_iter().flatten() {
                let Some(name) = entry.get(*key).and_then(serde_json::Value::as_str) else { continue };
                match seen.iter_mut().find(|(v, _)| v.get(*key).and_then(serde_json::Value::as_str) == Some(name)) {
                    Some((_, n)) => *n += 1,
                    None => seen.push((entry.clone(), 1)),
                }
            }
        }
    }
    let counted = |seen: Vec<(serde_json::Value, usize)>| -> Vec<serde_json::Value> {
        seen.into_iter()
            .map(|(mut v, n)| {
                if let Some(o) = v.as_object_mut() {
                    o.insert("count".into(), json!(n));
                }
                v
            })
            .collect()
    };
    let [(_, _, commands), (_, _, modes), (_, _, models), (_, _, efforts)] = merged;
    let agents: Vec<serde_json::Value> = picked
        .iter()
        .map(|i| json!({"id": i.id, "name": i.name, "machine": i.target.strip_prefix("agent:").unwrap_or(&i.target), "kind": i.kind, "status": i.status}))
        .collect();
    Ok(srv::json(
        StatusCode::OK,
        &json!({"agents": agents, "machines": machines, "commands": counted(commands), "modes": counted(modes), "models": counted(models), "efforts": counted(efforts)}),
    ))
}

async fn harness_command(hub: &Hub, id: &str, req: Request) -> R {
    let c: crate::harness::model::Command = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    match hub.harness.command(hub, id, c).await {
        crate::harness::registry::Sent::Ok => Ok(srv::status(StatusCode::NO_CONTENT)),
        crate::harness::registry::Sent::NoSession => Err(err(StatusCode::NOT_FOUND, "no such agent session")),
        crate::harness::registry::Sent::Unreachable => Err(err(StatusCode::CONFLICT, "the session's machine is not connected")),
    }
}

/// A file for a message (larger than fits inline): kept briefly, sent with the message that
/// names its `file_id`. `?name=&mime=`; the body is the file.
async fn harness_upload(hub: &Hub, q: &HashMap<String, String>, req: Request) -> R {
    let limit = usize::try_from(crate::harness::host::ATTACHMENT_MAX).unwrap_or(usize::MAX);
    let data = srv::body_bytes(req.into_body(), limit).await.map_err(|e| err(StatusCode::PAYLOAD_TOO_LARGE, format!("{e:#}")))?;
    if data.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "empty file"));
    }
    let name = q.get("name").cloned().unwrap_or_else(|| "file".to_owned());
    let mime = q.get("mime").cloned().unwrap_or_else(|| "application/octet-stream".to_owned());
    let size = data.len();
    let id = hub.harness.upload(name, mime, data).map_err(|e| err(StatusCode::INSUFFICIENT_STORAGE, format!("{e:#}")))?;
    Ok(srv::json(StatusCode::OK, &json!({"file_id": id, "size": size})))
}

#[derive(Deserialize)]
struct HarnessBroadcast {
    ids: Vec<String>,
    command: crate::harness::model::Command,
}

/// `POST /api/harness/command`: one command for the sessions listed, through the same sending as
/// `/api/harness/broadcast` (each send its own message id); `sent` lists those it reached, and the
/// rest of the report says why the others weren't.
async fn harness_command_many(hub: &Arc<Hub>, req: Request) -> R {
    use crate::harness::broadcast::{Broadcast, To};
    let b: HarnessBroadcast = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let command = serde_json::to_value(&b.command).map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let to = To { ids: b.ids, ..To::default() };
    let report = crate::harness::broadcast::run(hub, &Broadcast { to, text: None, mode: None, command: Some(command), approvals: None })
        .await
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let sent: Vec<&str> = report.results.iter().filter(|o| o.result == "sent").map(|o| o.id.as_str()).collect();
    Ok(srv::json(StatusCode::OK, &json!({ "sent": sent, "report": report })))
}

/// After a snapshot on an agent session's socket: what was resent is complete.
const SYNCED: &str = r#"{"t":"synced"}"#;

fn harness_ws(hub: Arc<Hub>, id: String, req: Request) -> R {
    let (resp, upgrade) = ws::accept(req).map_err(|r| ApiError(r.status, r.reason.into()))?;
    crate::rt::spawn(async move {
        let Ok(socket) = upgrade.await else { return };
        if let Err(e) = harness_attach(hub, id, socket).await {
            tracing::debug!(error = %format!("{e:#}"), "agent session websocket ended");
        }
    });
    Ok(resp)
}

/// A viewer of one agent session: the snapshot, then live updates (JSON frames tagged `t`); the
/// viewer sends commands (JSON tagged `type`). `{"t": "synced"}` follows each snapshot sent here
/// (on attach, and again after falling behind): the history resent since its `open` is complete,
/// so a viewer can swap it in at once instead of clearing what it shows.
async fn harness_attach(hub: Arc<Hub>, id: String, socket: ws::WebSocket) -> anyhow::Result<()> {
    use futures::FutureExt as _;
    let (mut tx, mut rx) = socket.split();
    let Some(att) = hub.harness.attach(&id) else {
        tx.send(ws::text(json!({"t": "closed"}).to_string())).await?;
        return Ok(());
    };
    for f in att.frames {
        tx.send(ws::text(f)).await?;
    }
    tx.send(ws::text(SYNCED)).await?;
    let mut live = att.live;
    loop {
        futures::select_biased! {
            f = live.recv().fuse() => match f {
                Ok(frame) => tx.send(ws::text(frame.as_ref())).await?,
                Err(async_broadcast::RecvError::Overflowed(_)) => {
                    // Fell behind: start over from the current state (it begins with "open").
                    for f in hub.harness.frames(&id).unwrap_or_default() {
                        tx.send(ws::text(f)).await?;
                    }
                    tx.send(ws::text(SYNCED)).await?;
                }
                Err(async_broadcast::RecvError::Closed) => return Ok(()),
            },
            m = rx.next().fuse() => match m {
                Some(Ok(Message::Text(t))) => match serde_json::from_str::<crate::harness::model::Command>(t.as_str()) {
                    Ok(c) => match hub.harness.command(&hub, &id, c).await {
                        crate::harness::registry::Sent::Ok => {}
                        crate::harness::registry::Sent::NoSession => tx.send(ws::text(json!({"t": "error", "error": "this session is gone"}).to_string())).await?,
                        crate::harness::registry::Sent::Unreachable => {
                            tx.send(ws::text(json!({"t": "error", "error": "the session's machine is not connected"}).to_string())).await?;
                        }
                    },
                    Err(e) => tx.send(ws::text(json!({"t": "error", "error": format!("bad command: {e}")}).to_string())).await?,
                },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return Ok(()),
                _ => {}
            },
        }
    }
}

// ------------------------------------------------------------------ folders

/// The folders in a folder on a machine, for the console's folder picker: `target` is `local`
/// (this server) or a machine (`<name>` or `agent:<name>`), `?path=` as typed. What can't be listed
/// (no such folder, permission denied) is said in a 200 answer; see `crate::dirs`. Folders are
/// offered where agents may run: where terminals are on.
async fn machine_dirs(hub: &Hub, target: &str, q: &HashMap<String, String>) -> R {
    let path = q.get("path").cloned().unwrap_or_default();
    if target == "local" {
        if !hub.cfg.terminal.enabled {
            return Err(err(StatusCode::FORBIDDEN, "terminals (and so agents) are disabled on this server"));
        }
        return Ok(srv::json(StatusCode::OK, &crate::dirs::here(path).await));
    }
    let machine = target.strip_prefix("agent:").unwrap_or(target);
    let (connected, agents) = hub.agents.lock().get(machine).map_or((false, false), |a| (a.connected, a.terminal && !a.ephemeral));
    if !connected {
        return Err(err(StatusCode::CONFLICT, format!("{machine} is not connected")));
    }
    if !agents {
        return Err(err(StatusCode::FORBIDDEN, format!("{machine} has terminals (and so agents) turned off")));
    }
    let json = match hub.dir_list(machine, &path).await {
        Ok(Some(json)) => json,
        Ok(None) => return Err(err(StatusCode::CONFLICT, format!("{machine}'s runner is too old to list folders: upgrade it"))),
        Err(e) => return Err(err(StatusCode::BAD_GATEWAY, format!("{e:#}"))),
    };
    let listing: crate::dirs::Listing =
        serde_json::from_str(&json).map_err(|e| err(StatusCode::BAD_GATEWAY, format!("{machine} sent an unreadable folder list: {e}")))?;
    Ok(srv::json(StatusCode::OK, &listing))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_downloads() {
        assert!(!TARGET.is_empty());
        let v = env!("CARGO_PKG_VERSION");
        assert_eq!(
            release_url("x86_64-pc-windows-msvc"),
            format!("https://github.com/hyper-light/kiln/releases/download/v{v}/kiln-{v}-x86_64-pc-windows-msvc.exe")
        );
        assert!(release_url("aarch64-apple-darwin").ends_with(&format!("kiln-{v}-aarch64-apple-darwin")));
        for script in [include_str!("../assets/install-agent.sh"), include_str!("../assets/install-agent.ps1")] {
            assert!(script.contains("@@KILN_SERVER@@"));
            assert!(script.contains("/dl/kiln/"));
        }
    }

    #[test]
    fn install_scripts_run_an_outpost_without_installing_it() {
        let sh = include_str!("../assets/install-agent.sh");
        assert!(sh.contains("--outpost) OUTPOST=1"));
        assert!(sh.contains(r#"exec "$BIN_DIR/kiln" outpost --server "$SERVER" --state-dir "$BIN_DIR""#));
        let ps1 = include_str!("../assets/install-agent.ps1");
        assert!(ps1.contains("[switch]$Outpost"));
        assert!(ps1.contains("@('outpost', '--server', $Server, '--state-dir', $dir)"));
    }
}
