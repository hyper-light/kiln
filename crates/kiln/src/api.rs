//! HTTP + WebSocket API for the UI and CLI (see docs/api.md), plus the small set of endpoints
//! agents use over HTTP: enrollment, join requests (`kiln join`), source archives and artifact
//! upload. Served by kiln's own HTTP layer (hyper's protocol code on hyper-rt).
//!
//! Auth is stateless so it survives restarts: sessions and one-time login links are HMACs of an
//! expiry under the admin token; job tokens are HMACs of the job id.

use std::borrow::Cow;
use std::collections::HashMap;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
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
use crate::wire::ui::{self, Came, LogDown, SessionDown, SessionFrame, SessionUp, StreamDown, TerminalDown, TerminalUp, Up, WebrtcDown};
use crate::wire::unread::{Page, Wire};

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
    e > crate::util::now_unix() && signed(hub, purpose, exp, sig)
}

/// Whether `sig` is [`sign`]'s signature of `payload` for `purpose`, worked out as the signature is made (the HMAC of
/// `purpose:payload` under the admin token, in hex) but on the stack: checking a session's cookie, which every request
/// a signed-in browser sends does, made a string to sign, the signature's hex and a copy of the cookie (goal 9, plan
/// 01a1a87b2cec step 5: authorization's work a request).
fn signed(hub: &Hub, purpose: &str, payload: &str, sig: &str) -> bool {
    use hmac::{KeyInit as _, Mac as _};
    let Ok(mut mac) = hmac::Hmac::<sha2::Sha256>::new_from_slice(hub.admin_token.as_bytes()) else { return false };
    mac.update(purpose.as_bytes());
    mac.update(b":");
    mac.update(payload.as_bytes());
    let mut hexed = [0u8; 64];
    hex::encode_to_slice(mac.finalize().into_bytes(), &mut hexed).is_ok() && crate::util::ct_eq(sig.as_bytes(), &hexed)
}

pub fn otp_url(hub_admin_token: &str, public_url: &str) -> String {
    let exp = crate::util::now_unix() + OTP_TTL;
    let sig = crate::util::hmac_hex(hub_admin_token.as_bytes(), format!("otp:{exp}").as_bytes());
    format!("{}/api/auth/otp?code={exp}.{sig}", public_url.trim_end_matches('/'))
}

pub fn job_token(admin_token: &str, job: &str) -> String {
    crate::util::hmac_hex(admin_token.as_bytes(), format!("job:{job}").as_bytes())
}

/// Cookie `name`'s value, as the request carries it.
fn cookie<'h>(headers: &'h HeaderMap, name: &str) -> Option<&'h str> {
    headers.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(';')).filter_map(|kv| kv.trim().split_once('=')).find(|(k, _)| *k == name).map(|(_, v)| v)
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
    cookie(headers, SESSION_COOKIE).is_some_and(|c| verify_expiring(hub, "session", c))
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
/// session (docs/ci.md W10-R01). The owner is told at once; only a check past that is a future boxed
/// apart (goal 9, plan 01a1a87b2cec step 5: the router boxed a future for every request's check).
async fn caller_of(hub: &Hub, headers: &HeaderMap) -> Option<crate::authz::Caller> {
    if authed(hub, headers) {
        return Some(crate::authz::Caller::owner());
    }
    boxed(|| crate::authz::gate::caller(hub, bearer(headers), cookie(headers, SESSION_COOKIE))).await
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
        || srv::secure(req)
        || srv::header_str(req, &header::HOST).is_some_and(|h| hub.front_doors.serves(host_only(h)))
}

// ------------------------------------------------------------------ router

/// The API as an HTTP handler.
pub struct Api {
    pub hub: Arc<Hub>,
}

impl srv::Handler for Api {
    async fn handle(self: Arc<Self>, req: Request) -> Response {
        // A shard behind its calm answers what it sheds at once, before the body is read (goal 6, plan
        // 01a1a59c2abe step 4).
        if let Some(shed) = crate::overload::shed_request(&req) {
            return shed;
        }
        let hub = self.hub.clone();
        // How long it waited before this handler started, from its head's arrival off its socket,
        // and how long it then took, by its class (goal 1, plan 01a1a59c2a68 step 2).
        let began = Instant::now();
        let upgrade = req.headers().get(header::UPGRADE).is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"));
        let class = crate::route_latency::class_of(req.method(), req.uri().path(), upgrade);
        let waited = req.extensions().get::<kiln_http::Received>().map(|r| began.saturating_duration_since(r.0));
        // How its answer goes out (compressed, validated): read before the route takes the request (W21-R15).
        let asked = crate::http::compress::Asked::of(&req);
        // Its polls labeled by its route, should one hold its shard (goal 9, plan 01a1a87b2cec step 2). The
        // router's state is boxed once, so the layers it's carried in (its label, its placement, the
        // connection's slot for its handler) each hold a pointer to it, not a copy of it: each took the whole
        // of what it carried as its own, and a request's future, which kiln-http boxes anew for each request
        // on a connection that idled between them (the lab's readers'), was three times the router's (goal
        // 10, plan 01a1a87b2dae step 5).
        let label = crate::route_latency::label_of(req.method(), req.uri().path(), upgrade);
        let timing = req.headers().contains_key(ASK_TIMING);
        let resp = match crate::rt::labeled(label, boxed(|| route(hub, req))).await {
            Ok(r) => r,
            Err(e) => e.into_response(),
        };
        // Finished here where its work fits the step's quantum (an answer left as it is, a small body validated),
        // with no future made for it (goal 9, plan 01a1a87b2cec step 5); a body long to hash or compress goes to the
        // compute pool in a future boxed as the route is (goal 10, plan 01a1a87b2dae step 5: the layers a request is
        // carried in hold a pointer to it, not its compressing).
        let resp = match crate::http::compress::finish_here(asked, resp) {
            crate::http::compress::Finished::Now(resp) => resp,
            crate::http::compress::Finished::Later(later) => boxed(|| crate::http::compress::finish_later(later)).await,
        };
        // Its compressing counted in what it took, as W21-R15 observed it, for its class and its own timing alike.
        let took = began.elapsed();
        if let Some(class) = class {
            crate::route_latency::observe(class, waited, took);
        }
        if timing { with_timing(resp, waited, took) } else { resp }
    }
}

/// The request header that asks for the request's own timing on its answer: [`with_timing`]'s `Server-Timing`.
/// Only a request that asks is told (kiln-lab's readers: its `split` numbers), so every other answer is as it was.
/// Goal 10, plan 01a1a87b2dae step 5: the server's wait and took at p99.9 are each a class's, and say nothing of
/// what one slow read spent where; one read's own split tells the server's share of it from the hops' and the
/// transfer's.
pub const ASK_TIMING: &str = "kiln-timing";

/// `resp` saying how long its request waited before its handler started (`wait`, from its head's arrival off its
/// socket, where the connection said that) and how long the handler then took (`took`), as `crate::route_latency`
/// observes them: W3C Server-Timing, `wait;dur=0.123, took;dur=0.045`, milliseconds to the microsecond.
fn with_timing(mut resp: Response, waited: Option<Duration>, took: Duration) -> Response {
    use std::io::Write as _;
    let ms = |d: Duration| d.as_secs_f64() * 1e3;
    // Said on the stack, its one copy the header's own (goal 9, plan 01a1a87b2cec step 5: a string was made for it
    // first). Room for two of the longest durations, in milliseconds (`u64::MAX` seconds: 23 digits, the point and 3
    // more), and the words between.
    let mut said = [0u8; 2 * 27 + 32];
    let room = said.len();
    let mut out = &mut said[..];
    let wrote = match waited {
        Some(w) => write!(out, "wait;dur={:.3}, took;dur={:.3}", ms(w), ms(took)),
        None => write!(out, "took;dur={:.3}", ms(took)),
    };
    let len = room.saturating_sub(out.len());
    if wrote.is_ok()
        && let Some(v) = said.get(..len).and_then(|s| header::HeaderValue::from_bytes(s).ok())
    {
        resp.headers_mut().insert(header::HeaderName::from_static("server-timing"), v);
    }
    resp
}

/// `path`'s segments, each percent-decoded (and `+` a space) where it has anything to decode, else as it
/// is in the path: made once a request, by the router, and handed on (goal 10, plan 01a1a87b2dae step 5:
/// each request split its path twice, two copies a segment each time).
fn segments(path: &str) -> Vec<Cow<'_, str>> {
    path.trim_matches('/').split('/').filter(|s| !s.is_empty()).map(|s| if s.contains(['%', '+']) { Cow::Owned(srv::decode(s)) } else { Cow::Borrowed(s) }).collect()
}

/// `seg` as the router matches them, in `room` when they fit ([`srv::ROUTE_DEPTH`]: every route's own do), else in a
/// vector of their own (goal 9, plan 01a1a87b2cec step 5: the router and its authorized half each made one a request).
fn strs<'r>(seg: &'r [Cow<'_, str>], room: &'r mut [&'r str; srv::ROUTE_DEPTH]) -> Cow<'r, [&'r str]> {
    match room.get_mut(..seg.len()) {
        Some(slots) => {
            for (slot, s) in slots.iter_mut().zip(seg) {
                *slot = &**s;
            }
            Cow::Borrowed(slots)
        }
        None => Cow::Owned(seg.iter().map(|s| &**s).collect()),
    }
}

async fn route(hub: Arc<Hub>, req: Request) -> R {
    // A peered cluster's address (`peer-<slug>.<domain>`): every path there is the peer's.
    if let Some((f, slug)) = crate::peers::route(&hub, &req) {
        let secure = is_secure(&hub, &req);
        return Ok(boxed(|| f.serve(&slug, req, secure)).await);
    }
    // A peered cluster under this one's own address (`/.kiln-peer/<slug>/`, kiln reached by
    // address): its API forwarded, its pages this cluster's app (in the browser) drawing it.
    if let Some((f, slug, page)) = crate::peers::route_path(&hub, &req) {
        if page {
            return Ok(crate::ui::shell(&req));
        }
        let secure = is_secure(&hub, &req);
        return Ok(boxed(|| f.serve_path(&slug, req, secure)).await);
    }
    let method = req.method().clone();
    // The request's target, shared: its path is the bytes the request came in, counted once more, not copied
    // (goal 9, plan 01a1a87b2cec step 5).
    let target = req.uri().clone();
    let path = target.path();
    let seg = segments(path);
    let mut room = [""; srv::ROUTE_DEPTH];
    let s = strs(&seg, &mut room);

    // Pages' own address (`[server] pages_url`): pages agents show, and nothing of kiln's.
    if pages_site(&hub).is_some_and(|site| asks_for(&req, &site)) {
        return match (&method, &*s) {
            (&Method::GET | &Method::HEAD, ["v", "_", "fonts", name]) => Ok(page_font(name)),
            (&Method::GET | &Method::HEAD, ["v", id, ..]) => boxed(|| page(&hub, id, &req)).await,
            // An agent's app opened on a person's device (`crate::harness::opened`): its link's
            // code, then whatever its device asks of it.
            (&Method::GET, ["o", code]) => Ok(crate::harness::opened::enter(&hub, code)),
            _ => match crate::harness::opened::of_request(&hub, &req) {
                crate::harness::opened::Holds::Open(o) => Ok(boxed(|| crate::harness::opened::forward(&hub, &o, req)).await),
                crate::harness::opened::Holds::Ended => Ok(crate::harness::opened::ended()),
                crate::harness::opened::Holds::Nothing => Err(err(StatusCode::NOT_FOUND, "not found")),
            },
        };
    }

    // kiln's OCI registry (docs/ci.md W12-R01), with its own auth (Docker's token flow), as a task
    // of its own (an upload streams for as long as it takes).
    if hub.cfg.ci.enabled && crate::ci::registry::proxy::is_proxy(path) {
        let h = Arc::clone(&hub);
        return boxed(|| crate::rt::spawn(async move { boxed(|| crate::ci::registry::proxy::handle(&h, req)).await })).await.map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "the proxy's answer was lost"));
    }
    if hub.cfg.ci.enabled && crate::ci::registry::is_registry(path) {
        let h = Arc::clone(&hub);
        return boxed(|| crate::rt::spawn(async move { boxed(|| crate::ci::registry::handle(&h, req)).await })).await.map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "the registry's answer was lost"));
    }

    // Open routes (their own auth, if any).
    match (&method, &*s) {
        (&Method::POST, ["api", "mcp"]) => {
            // The owner, by the admin token, its session or an MCP token kiln minted; else a
            // user's session or token, whose operations the route table judges one by one.
            let caller = match crate::mcp::caller(&hub, req.headers(), authed(&hub, req.headers())) {
                // An agent session's own token: that agent, acting for whoever started it.
                Some(actor) => match boxed(|| crate::mcp::token_caller(&hub, &actor)).await {
                    Some(Ok(c)) => Some(c),
                    Some(Err(why)) => return Err(err(StatusCode::UNAUTHORIZED, why)),
                    None => {
                        let mut c = crate::authz::Caller::owner();
                        c.name = actor;
                        Some(c)
                    }
                },
                None => boxed(|| caller_of(&hub, req.headers())).await,
            };
            return match caller {
                Some(caller) => boxed(|| crate::mcp::http(&hub, caller, req.into_body())).await.map_err(|(c, m)| err(c, m)),
                None => Err(err(StatusCode::UNAUTHORIZED, "authentication required")),
            };
        }
        // A test build's (`--features chaos`): bytes the server never sends, sent to a runner as a
        // frame or as an item of its live command stream (`Hub::chaos_send`), for tests/hostile.rs.
        // The owner's alone. `{"runner", "lane", "item", "bytes": <base64>}`.
        #[cfg(feature = "chaos")]
        (&Method::POST, ["api", "chaos", "send"]) => {
            use base64::Engine as _;
            if !authed(&hub, req.headers()) {
                return Err(err(StatusCode::UNAUTHORIZED, "authentication required"));
            }
            let body: serde_json::Value = boxed(|| srv::body_json(req.into_body(), JSON_LIMIT)).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            let field = |k: &str| body.get(k).and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
            let lane = crate::proto::lane_named(&field("lane")).unwrap_or(crate::proto::LANE_CONTROL);
            let item = body.get("item").and_then(serde_json::Value::as_bool).unwrap_or(false);
            let bytes = base64::engine::general_purpose::STANDARD.decode(field("bytes")).map_err(|e| err(StatusCode::BAD_REQUEST, format!("bytes: {e}")))?;
            let runner = field("runner");
            return boxed(|| hub.chaos_send(&runner, lane, item, bytes)).await.map(|v| srv::json(StatusCode::OK, &v)).map_err(|why| err(StatusCode::CONFLICT, why));
        }
        // Every operation over HTTP (docs/ci.md W20-R01): the list, and one run as its caller.
        // Each is checked as its own route is, and a change audited as that route's would be.
        (&Method::GET, ["api", "ops"]) => {
            return match boxed(|| caller_of(&hub, req.headers())).await {
                Some(_) => Ok(srv::json(StatusCode::OK, &crate::ops::catalog(&hub))),
                None => Err(err(StatusCode::UNAUTHORIZED, "authentication required")),
            };
        }
        (&Method::POST, ["api", "ops", name]) => {
            let Some(caller) = boxed(|| caller_of(&hub, req.headers())).await else { return Err(err(StatusCode::UNAUTHORIZED, "authentication required")) };
            let name = (*name).to_owned();
            let input: serde_json::Value = boxed(|| srv::body_json(req.into_body(), JSON_LIMIT)).await.unwrap_or(serde_json::Value::Null);
            let cx = crate::ops::Cx { hub: Arc::clone(&hub), caller };
            return boxed(|| crate::ops::call(cx, &name, input)).await.map(|v| srv::json(StatusCode::OK, &v)).map_err(|(c, m)| err(c, m));
        }
        (&Method::POST, ["api", "auth", "login"]) => return boxed(|| login(&hub, req)).await,
        // SSO with GitHub, GitLab and Bitbucket (docs/ci.md W23-R08).
        (&Method::GET, ["api", "auth", "sso"]) => return Ok(srv::json(StatusCode::OK, &boxed(|| crate::authz::sso::providers(&hub)).await)),
        (&Method::GET, ["api", "auth", provider, "start"]) => {
            let q = srv::query(&req);
            return match boxed(|| crate::authz::sso::start(&hub, provider, q.get("return").map(String::as_str))).await {
                Ok(to) => Ok(srv::with_header(srv::status(StatusCode::SEE_OTHER), header::LOCATION, &to)),
                Err((c, m)) => Err(err(c, m)),
            };
        }
        (&Method::GET, ["api", "auth", provider, "callback"]) => {
            let q = srv::query(&req);
            let (code, state) = (q.get("code").cloned().unwrap_or_default(), q.get("state").cloned().unwrap_or_default());
            return match boxed(|| crate::authz::sso::callback(&hub, provider, &code, &state)).await {
                Ok((session, _, back)) => {
                    let r = srv::with_header(srv::status(StatusCode::SEE_OTHER), header::LOCATION, &back);
                    Ok(srv::with_header(r, header::SET_COOKIE, &cookie_for(&session, is_secure(&hub, &req))))
                }
                Err((c, m)) => Err(err(c, m)),
            };
        }
        (&Method::GET, ["api", "auth", "otp"]) => return Ok(boxed(|| otp(&hub, &req)).await),
        (&Method::GET, ["api", "auth", "methods"]) => return Ok(boxed(|| auth_methods(&hub)).await),
        (&Method::POST, ["api", "auth", "password"]) => return boxed(|| login_password(&hub, req)).await,
        // Passkeys (docs/ci.md W10): a challenge, then the signature that answers it.
        (&Method::POST, ["api", "auth", "passkey", "options"]) => {
            let origin = req.headers().get(header::ORIGIN).and_then(|v| v.to_str().ok()).map(str::to_owned);
            return crate::authz::passkeys::sign_in_options(&hub, origin.as_deref()).map(|v| srv::json(StatusCode::OK, &v)).map_err(|(c, m)| err(c, m));
        }
        (&Method::POST, ["api", "auth", "passkey"]) => return boxed(|| login_passkey(&hub, req)).await,
        (&Method::POST, ["api", "auth", "logout"]) => return Ok(logout()),
        (&Method::GET, ["api", "me"]) => return Ok(boxed(|| me(&hub, req.headers())).await),
        (&Method::POST, ["api", "agents", "enroll"]) => return boxed(|| enroll(&hub, req)).await,
        (&Method::POST, ["api", "agents", "instance"]) => return boxed(|| enroll_instance(&hub, req)).await,
        (&Method::POST, ["api", "agents", "rekey"]) => return boxed(|| rekey_agent(&hub, req)).await,
        (&Method::GET, ["api", "agents", "tunnel"]) => return agent_tunnel(&hub, req),
        (&Method::POST, ["api", "join-requests"]) => return boxed(|| create_join(&hub, req)).await,
        (&Method::GET, ["api", "join-requests", id]) => {
            let id = (*id).to_owned();
            return boxed(|| poll_join(&hub, &id, req.headers())).await;
        }
        (&Method::GET, ["api", "join", "invited"]) => return Ok(join_invited(&hub, &req)),
        (&Method::GET, ["api", "jobs", id, "source"]) => return boxed(|| job_source(&hub, id, req.headers())).await,
        (&Method::PUT, ["api", "jobs", id, "artifacts", "upload"]) => {
            let id = (*id).to_owned();
            return boxed(|| put_artifacts(&hub, &id, req)).await;
        }
        // An upload in pieces (`crate::exec`): how much of it came, and its end, checked.
        (&Method::GET, ["api", "jobs", id, "artifacts", "upload"]) => {
            let id = (*id).to_owned();
            return boxed(|| artifacts_had(&hub, &id, req.headers())).await;
        }
        (&Method::POST, ["api", "jobs", id, "artifacts", "upload"]) => {
            let id = (*id).to_owned();
            return boxed(|| artifacts_done(&hub, &id, req)).await;
        }
        (&Method::POST, ["api", "hooks", "github"]) => return boxed(|| github_hook(&hub, req)).await,
        // The GitHub App's events and GitHub's return from making it (W14-R10): open, they carry
        // their own proof (a signature; a state kiln handed out).
        (&Method::POST, ["api", "hooks", "github", "app"]) => return boxed(|| crate::githost::web::webhook(&hub, req)).await.map_err(|(c, m)| err(c, m)),
        // GitLab's and Bitbucket's webhooks, checked with their secrets (docs/ci.md W23-R04, R05).
        (&Method::POST, ["api", "hooks", "gitlab"]) => return boxed(|| crate::githost::hooks::gitlab(&hub, req)).await.map_err(|(c, m)| err(c, m)),
        (&Method::POST, ["api", "hooks", "bitbucket"]) => return boxed(|| crate::githost::hooks::bitbucket(&hub, req)).await.map_err(|(c, m)| err(c, m)),
        (&Method::GET, ["api", "ci", "github", "app", "callback"]) => return boxed(|| crate::githost::web::callback(&hub, &req)).await.map_err(|(c, m)| err(c, m)),
        (&Method::GET, ["api", "shared", "plans", token]) => return boxed(|| shared_plan(&hub, token)).await,
        (&Method::GET | &Method::HEAD, ["api", "sync", "blobs", hash]) => {
            let hash = (*hash).to_owned();
            return sync_blob_get(&hub, &hash, &req);
        }
        // A copy of kiln.db for a runner's learner of the cluster group (docs/p2p.md, P2).
        (&Method::GET, ["api", "cluster", "copy"]) => return boxed(|| cluster_copy(&hub, &req)).await,
        // Whether a request's credentials are the owner's: a runner's front door answers the owner's
        // reads from its replica, and passes everyone else's here (docs/p2p.md, P2).
        (&Method::GET, ["api", "cluster", "owner"]) => return Ok(srv::json(StatusCode::OK, &json!({ "owner": authed(&hub, req.headers()) }))),
        // The local store's presigned URLs (docs/blobs.md): the URL is the credential.
        (&Method::GET | &Method::HEAD | &Method::PUT, ["api", "blobs", _, ..]) => {
            let encoded = path.strip_prefix("/api/blobs/").unwrap_or_default().to_owned();
            return boxed(|| presigned(&hub, &encoded, req)).await;
        }
        (&Method::PUT, ["api", "sync", "blobs", hash]) => {
            let hash = (*hash).to_owned();
            return boxed(|| sync_blob_put(&hub, &hash, req)).await;
        }
        (&Method::GET, ["api", "sync", "bundles", batch]) => {
            let batch = (*batch).to_owned();
            return sync_bundle_get(&hub, &batch, &req);
        }
        (&Method::PUT, ["api", "sync", "bundles", batch]) => {
            let batch = (*batch).to_owned();
            return boxed(|| sync_bundle_put(&hub, &batch, req)).await;
        }
        // OTLP/HTTP (`crate::observe::otlp`), with the ingest token or the admin's.
        (&Method::POST, ["v1", "metrics"]) => {
            let ok = bearer(req.headers()).is_some_and(|t| crate::observe::api::ingest_allowed(t, &hub.admin_token));
            return Ok(boxed(|| crate::observe::api::otlp(crate::observe::otlp::Signal::Metrics, req, ok)).await);
        }
        (&Method::POST, ["v1", "logs"]) => {
            let ok = bearer(req.headers()).is_some_and(|t| crate::observe::api::ingest_allowed(t, &hub.admin_token));
            return Ok(boxed(|| crate::observe::api::otlp(crate::observe::otlp::Signal::Logs, req, ok)).await);
        }
        (&Method::POST, ["v1", "traces"]) => {
            let ok = bearer(req.headers()).is_some_and(|t| crate::observe::api::ingest_allowed(t, &hub.admin_token));
            return Ok(boxed(|| crate::observe::api::otlp(crate::observe::otlp::Signal::Traces, req, ok)).await);
        }
        // Prometheus's remote write (`crate::observe::remote`): the same token, as a bearer token
        // or basic auth's password.
        (&Method::POST, ["api", "v1", "write"]) => {
            let ok = crate::observe::remote::token(req.headers()).is_some_and(|t| crate::observe::api::ingest_allowed(&t, &hub.admin_token));
            if !ok {
                return Ok(srv::text(StatusCode::UNAUTHORIZED, "a remote write sends the ingest token: `authorization: credentials`, `bearer_token`, or basic auth's password"));
            }
            return Ok(match crate::observe::api::get() {
                Some(o) => boxed(|| crate::observe::remote::answer(o, req)).await,
                None => srv::with_header(srv::text(StatusCode::SERVICE_UNAVAILABLE, "observe is starting"), header::RETRY_AFTER, "1"),
            });
        }
        // Datadog's Agent (`crate::observe::ddagent`) at Datadog's own paths, its API key the same
        // token: what Datadog answers a key that is not one.
        (_, segs) if crate::observe::ddagent::is_datadog(segs) => {
            let ok = crate::observe::ddagent::api_key(&req).is_some_and(|k| crate::observe::api::ingest_allowed(&k, &hub.admin_token));
            if !ok {
                return Ok(srv::json(StatusCode::FORBIDDEN, &serde_json::json!({ "errors": ["Forbidden", "API key is missing or invalid"] })));
            }
            return Ok(match crate::observe::api::get() {
                Some(o) => boxed(|| crate::observe::ddagent::answer(o, segs, req)).await,
                None => srv::with_header(srv::text(StatusCode::SERVICE_UNAVAILABLE, "observe is starting"), header::RETRY_AFTER, "1"),
            });
        }
        // OTLP/gRPC (`crate::observe::otlp::grpc`): the same token, as gRPC's metadata.
        (&Method::POST, [service, "Export"]) if crate::observe::otlp::grpc_signal(&format!("/{service}/Export")).is_some() => {
            let ok = bearer(req.headers()).is_some_and(|t| crate::observe::api::ingest_allowed(t, &hub.admin_token));
            let signal = crate::observe::otlp::grpc_signal(&format!("/{service}/Export")).unwrap_or(crate::observe::otlp::Signal::Metrics);
            return Ok(boxed(|| crate::observe::api::otlp_grpc(signal, req, ok)).await);
        }
        // The clouds' pushes (`crate::observe::cloud::push`), with the ingest token or the admin's:
        // Firehose sends it as its access key, a Pub/Sub push subscription in its endpoint's query.
        (&Method::POST, ["v1", "firehose"]) => {
            let ok = req.headers().get("x-amz-firehose-access-key").and_then(|v| v.to_str().ok()).is_some_and(|t| crate::observe::api::ingest_allowed(t, &hub.admin_token));
            return Ok(boxed(|| crate::observe::cloud::push::firehose(req, ok)).await);
        }
        (&Method::POST, ["v1", "pubsub"]) => {
            let ok = srv::query(&req).get("token").is_some_and(|t| crate::observe::api::ingest_allowed(t, &hub.admin_token));
            return Ok(boxed(|| crate::observe::cloud::push::pubsub(req, ok)).await);
        }
        // A snapshot's file for a runner's replica, with that machine's token (`crate::observe`).
        (&Method::GET, ["api", "observe", "v1", "snapshot", snap, file]) => return crate::observe::api::snapshot(snap, file, &req, |m| hub.machine_of(m)).map_err(|(c, m)| err(c, m)),
        (&Method::GET, ["metrics"]) => return Ok(metrics_handler(&hub, req.headers()).await),
        (&Method::GET, ["healthz"]) => return Ok(srv::text(StatusCode::OK, "ok")),
        // The browser protocol's lab page, on a lab's server alone (`KILN_LIVE_LAB`, `crate::live`).
        (&Method::GET, ["__live", file]) if crate::live::lab_dir().is_some() => return Ok(boxed(|| crate::live::lab_file(file)).await),
        (&Method::GET, ["readyz"]) => return Ok(boxed(|| readyz(&hub)).await),
        (&Method::GET, ["healthz", "server"]) => return Ok(server_health(&hub)),
        (&Method::GET, ["healthz", "delay"]) => return Ok(shard_delay()),
        (&Method::GET, ["dl", "kiln"]) => return boxed(download_binary).await,
        (&Method::GET, ["dl", "kiln", target]) => return boxed(|| download_for(&hub, target)).await,
        (&Method::GET, ["dl", "kiln", target, build]) => return boxed(|| download_build(&hub, target, build)).await,
        // A machine building kiln itself (`crate::fleet`), with its build's token.
        (&Method::GET, ["api", "updates", "jobs", id, "source"]) => return boxed(|| build_source(&hub, id, req.headers())).await,
        (&Method::POST, ["api", "updates", "jobs", id, "binary"]) => {
            let id = (*id).to_owned();
            let q = srv::query(&req);
            return boxed(|| build_binary(&hub, &id, &q, req)).await;
        }
        (&Method::GET, ["install.sh"]) => return Ok(install_script(&hub)),
        (&Method::GET, ["install.ps1"]) => return Ok(install_powershell(&hub)),
        // kiln's CA for devices (it is public): PEM, DER, and an Apple configuration profile.
        (&Method::GET, ["ca.pem"]) => return Ok(crate::frontdoor::control::ca(&hub, "pem")),
        (&Method::GET, ["ca.crt"]) => return Ok(crate::frontdoor::control::ca(&hub, "crt")),
        (&Method::GET, ["ca.mobileconfig"]) => return Ok(crate::frontdoor::control::ca(&hub, "mobileconfig")),
        // Pages agents show in their conversations (`crate::harness::visuals`): open, each at an
        // address nobody can guess, sandboxed so their scripts run away from kiln and the viewer.
        // With an address of their own, pages are served only there.
        (&Method::GET | &Method::HEAD, ["v", ..]) if hub.cfg.server.pages_url.is_some() => return Ok(to_pages(&hub, &req)),
        (&Method::GET | &Method::HEAD, ["v", "_", "fonts", name]) => return Ok(page_font(name)),
        (&Method::GET | &Method::HEAD, ["v", id, ..]) => return boxed(|| page(&hub, id, &req)).await,
        _ => {}
    }
    // CI's open routes: runtime services, job ID tokens, the OIDC issuer (docs/ci.md §2.6).
    if crate::ci::api::is_open(&s) {
        return match boxed(|| crate::ci::api::open(&hub, &s, req)).await {
            Some(r) => r.map_err(|(c, m)| err(c, m)),
            None => Err(err(StatusCode::NOT_FOUND, "not found")),
        };
    }

    let is_api = s.first() == Some(&"api");
    if !is_api {
        return Ok(boxed(|| crate::uiserver::serve(req)).await);
    }
    let Some(caller) = caller_of(&hub, req.headers()).await else {
        return Err(err(StatusCode::UNAUTHORIZED, "authentication required"));
    };
    if method == Method::GET || method == Method::HEAD {
        return boxed(|| authorized(hub, caller, req, seg)).await;
    }
    // A change is audited (W19-R05): one row for the request, refused or not, which takes what
    // its handlers record on the way.
    let (who, q, ip) = (caller.clone(), srv::query(&req), srv::remote_ip(&req));
    let h = Arc::clone(&hub);
    crate::audit::request(&h, crate::audit::Req { caller: &who, method: &method, path, q: &q, ip }, boxed(|| authorized(hub, caller, req, seg)), |r: &R| match r {
        Ok(resp) => resp.status().as_u16(),
        Err(e) => e.0.as_u16(),
    })
    .await
}

/// A read run as `caller` through the API's own routes and checks (`authorized`), for the browser
/// protocol's requests (`crate::live`, goal 3 of the browser protocol program): the answer's status and
/// body, a body past `most` answered 413. Only the API's reads, never the protocol's own endpoint.
pub(crate) async fn live_read(hub: Arc<Hub>, caller: crate::authz::Caller, path: &str, most: usize) -> (u16, Vec<u8>) {
    use http_body_util::BodyExt as _;
    if !path.starts_with("/api/") || path.starts_with(crate::live::PATH) {
        return (404, b"not a read the protocol carries".to_vec());
    }
    let Ok(req) = http::Request::builder().method(Method::GET).uri(path).body(kiln_http::Incoming::empty()) else {
        return (400, b"not a path".to_vec());
    };
    // Its segments, as the router splits a request's path for `authorized` (goal 10, plan 01a1a87b2dae step 5).
    let at = req.uri().path().to_owned();
    let seg = segments(&at);
    let resp = match boxed(|| authorized(hub, caller, req, seg)).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    };
    let status = resp.status().as_u16();
    match http_body_util::Limited::new(resp.into_body(), most).collect().await {
        Ok(c) => (status, c.to_bytes().to_vec()),
        Err(_) => (413, b"the answer is larger than a frame holds".to_vec()),
    }
}

/// A future made and boxed in a frame of its own. A debug build gives each temporary of an async
/// function its own stack slot, so a router awaiting many futures inline made one of every future
/// it could await, in its poll frame: `authorized`'s was 772 KiB, and a request that reached out
/// (a webhook's verify) overflowed a shard's 2 MiB stack. Boxed, each is a pointer there.
pub(crate) fn boxed<'a, F: Future + Send + 'a>(make: impl FnOnce() -> F) -> Pin<Box<dyn Future<Output = F::Output> + Send + 'a>> {
    Box::pin(make())
}

/// The API past authentication: everyone but the owner is held to the route table (W10-R05). `seg` is
/// the request's path in segments, as the router split it ([`segments`]).
async fn authorized(hub: Arc<Hub>, caller: crate::authz::Caller, req: Request, mut seg: Vec<Cow<'_, str>>) -> R {
    let method = req.method().clone();
    // A runner the path names by a name people see (its label, a name its certificate carried
    // from before) is the machine id kiln keys it by (docs/machines.md, "One record a machine").
    if let [api, kind, name, ..] = seg.as_mut_slice()
        && api == "api"
        && (kind == "runners" || kind == "agents")
        && !["enroll", "instance", "rekey", "tunnel", "join"].contains(&&**name)
        && let Some(id) = hub.machine_named(name)
    {
        *name = Cow::Owned(id);
    }
    let mut room = [""; srv::ROUTE_DEPTH];
    let s = strs(&seg, &mut room);
    let q = srv::query(&req);
    if !caller.is_owner() {
        boxed(|| crate::authz::gate::check(&hub, &caller, &method, &s, &q)).await.map_err(|(c, m)| err(c, m))?;
    }
    let mut req = req;
    req.extensions_mut().insert(caller.clone());
    // An agent session answers only to whoever reaches it (docs/agents.md, "Whose agents"): one the
    // caller doesn't see isn't found, and one it sees but doesn't act on refuses every change.
    // Whoever sees every session (the owner, a manager) reaches each: told at once, with no future made to ask
    // (goal 9, plan 01a1a87b2cec step 5: one was boxed for every read of an agent's page).
    if let ["api", "harness", "sessions", id, ..] = &*s
        && !crate::harness::lineage::sees_all(&caller)
    {
        let reached = if matches!(method, Method::GET | Method::HEAD) {
            boxed(|| crate::harness::lineage::may_see(&hub, &caller, id)).await
        } else {
            boxed(|| crate::harness::lineage::may_steer(&hub, &caller, id)).await
        };
        reached.map_err(|(c, m)| err(c, m))?;
    }
    if matches!(s.get(1), Some(&("users" | "groups" | "grants" | "roles" | "tokens"))) {
        return match boxed(|| crate::authz::api::route(&hub, &caller, &method, &s, &q, req)).await {
            Some(r) => r.map_err(|(c, m)| err(c, m)),
            None => Err(err(StatusCode::NOT_FOUND, "not found")),
        };
    }
    match (&method, &*s) {
        (&Method::GET, ["api", "openapi.json"]) => Ok(srv::json(StatusCode::OK, &crate::authz::openapi(env!("CARGO_PKG_VERSION"), &crate::ops::all(&hub)))),
        (&Method::GET, ["api", "peers"]) => boxed(|| crate::peers::list(&hub)).await.map_err(|(c, m)| err(c, m)),
        (&Method::GET, ["api", "peers", "open"]) => crate::peers::openable(&hub).map_err(|(c, m)| err(c, m)),
        (&Method::POST, ["api", "peers"]) => boxed(|| crate::peers::register(&hub, &caller.name, req)).await.map_err(|(c, m)| err(c, m)),
        (&Method::POST, ["api", "peers", "authorize"]) => boxed(|| crate::peers::authorize(&hub, &caller.name, req)).await.map_err(|(c, m)| err(c, m)),
        (&Method::POST, ["api", "peers", id, "complete"]) => {
            let id = (*id).to_owned();
            boxed(|| crate::peers::complete(&hub, &caller.name, &id, req)).await.map_err(|(c, m)| err(c, m))
        }
        (&Method::POST, ["api", "peers", id, "challenge"]) => boxed(|| crate::peers::rechallenge(&hub, &caller.name, id)).await.map_err(|(c, m)| err(c, m)),
        (&Method::POST, ["api", "peers", id, "frame"]) => {
            let host = srv::header_str(&req, &header::HOST).unwrap_or_default().to_owned();
            crate::peers::frame(&hub, &caller, id, &host, is_secure(&hub, &req)).map_err(|(c, m)| err(c, m))
        }
        (&Method::PUT, ["api", "peers", id]) => {
            let id = (*id).to_owned();
            boxed(|| crate::peers::readdress(&hub, &caller.name, &id, req)).await.map_err(|(c, m)| err(c, m))
        }
        (&Method::DELETE, ["api", "peers", id]) => boxed(|| crate::peers::remove(&hub, &caller.name, id)).await.map_err(|(c, m)| err(c, m)),
        // Ship's read model: a repository's commits and where each stands (plan 01a175b3a21174bc8769f5e784b90ecb).
        (&Method::GET, ["api", "ship", "commits"]) => boxed(|| crate::ship::list(&hub, &q, Some(&caller))).await.map_err(|(c, m)| err(c, m)),
        (&Method::GET, ["api", "ship", "commits", repo, sha]) => boxed(|| crate::ship::get(&hub, repo, sha, Some(&caller))).await.map_err(|(c, m)| err(c, m)),
        (&Method::GET, ["api", "ship", "commits", repo, sha, "diff"]) => boxed(|| crate::ci::diffs::ship_commit_diff(&hub, repo, sha, &caller)).await.map_err(|(c, m)| err(c, m)),
        (&Method::GET, ["api", "ship", "commits", repo, sha, "blob"]) => boxed(|| crate::ci::diffs::ship_commit_blob(&hub, repo, sha, &q, &caller)).await.map_err(|(c, m)| err(c, m)),
        (&Method::GET, ["api", "ship", "commits", owner, name, sha]) => {
            let repo = format!("{owner}/{name}");
            boxed(|| crate::ship::get(&hub, &repo, sha, Some(&caller))).await.map_err(|(c, m)| err(c, m))
        }
        (&Method::GET, ["api", "ship", "commits", owner, name, sha, "diff"]) => {
            let repo = format!("{owner}/{name}");
            boxed(|| crate::ci::diffs::ship_commit_diff(&hub, &repo, sha, &caller)).await.map_err(|(c, m)| err(c, m))
        }
        (&Method::GET, ["api", "ship", "commits", owner, name, sha, "blob"]) => {
            let repo = format!("{owner}/{name}");
            boxed(|| crate::ci::diffs::ship_commit_blob(&hub, &repo, sha, &q, &caller)).await.map_err(|(c, m)| err(c, m))
        }
        // A commit's jobs run again, paused, resumed or cancelled together (W26-R21, R22).
        (&Method::POST, ["api", "ship", "jobs"]) => {
            let body: serde_json::Value = boxed(|| srv::body_json(req.into_body(), JSON_LIMIT)).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
            boxed(|| crate::ship::jobs(&hub, &caller, &body)).await.map_err(|(c, m)| err(c, m))
        }
        // Ship's Deploys: every app by project, an app's pane, and the strip's verbs (W26-R24).
        (&Method::GET, ["api", "ship", "deploys"]) => boxed(|| crate::ship::deploys::list(&hub, &q, &caller)).await.map_err(|(c, m)| err(c, m)),
        (&Method::GET, ["api", "ship", "deploys", project, name]) => boxed(|| crate::ship::deploys::get(&hub, project, name, &caller)).await.map_err(|(c, m)| err(c, m)),
        (&Method::POST, ["api", "ship", "deploys"]) => {
            let body: serde_json::Value = boxed(|| srv::body_json(req.into_body(), JSON_LIMIT)).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
            boxed(|| crate::ship::deploys::act(&hub, &caller, &body)).await.map_err(|(c, m)| err(c, m))
        }
        // A row's Hand off: what the commit has open, to the agent that takes it (W26-R18).
        (&Method::POST, ["api", "ship", "hand-off"]) => {
            let body: serde_json::Value = boxed(|| srv::body_json(req.into_body(), JSON_LIMIT)).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
            boxed(|| crate::ship::hand_off(&hub, &caller, &body)).await.map_err(|(c, m)| err(c, m))
        }
        // What a person signs to hand it off from a browser, with the browser's own key (W27-R08).
        (&Method::POST, ["api", "ship", "hand-off", "statement"]) => {
            let body: serde_json::Value = boxed(|| srv::body_json(req.into_body(), JSON_LIMIT)).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
            boxed(|| crate::ship::hand_off_statement(&hub, &caller, &body)).await.map_err(|(c, m)| err(c, m))
        }
        // A row's Ready: an agent's draft marked ready for review, by its person (W26-R18).
        (&Method::POST, ["api", "ship", "ready"]) => {
            let body: serde_json::Value = boxed(|| srv::body_json(req.into_body(), JSON_LIMIT)).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
            boxed(|| crate::ship::ready(&hub, &caller, &body)).await.map_err(|(c, m)| err(c, m))
        }
        (_, ["api", "cd", rest @ ..]) => boxed(|| crate::cd::api::route(&hub, rest, &q, req)).await.map_err(|(c, m)| err(c, m)),
        (_, ["api", "deploy", rest @ ..]) => boxed(|| crate::deploy::api::route(&hub, rest, &q, req)).await.map_err(|(c, m)| err(c, m)),
        (&Method::GET, ["api", "ci", "legs", id, "terminal"]) => {
            let id = (*id).to_owned();
            boxed(|| leg_terminal_ws(hub.clone(), &id, q, &caller, req)).await
        }
        (_, ["api", "ci", rest @ ..]) => boxed(|| crate::ci::api::route(&hub, rest, &q, req)).await.map_err(|(c, m)| err(c, m)),
        (_, ["api", "push", rest @ ..]) => boxed(|| crate::push::api::route(&hub, &caller, &method, rest, req)).await.map_err(|(c, m)| err(c, m)),
        // W20-R11: experiments.
        (_, ["api", "experiments", rest @ ..]) => {
            let rest: Vec<&str> = rest.to_vec();
            let body: serde_json::Value = if method == Method::POST { boxed(|| srv::body_json(req.into_body(), JSON_LIMIT)).await.unwrap_or(serde_json::Value::Null) } else { serde_json::Value::Null };
            boxed(|| crate::ci::experiments::route(&hub, &caller, &method, &rest, &q, body)).await.map_err(|(c, m)| err(c, m))
        }
        // Observe's routes as a task of their own: its handlers reach out (a cloud's pull, a
        // synthetic test) and nest deep, and in a debug build the routers' frames under them
        // (each arm's temporaries a slot of its own) took a shard's 2 MiB stack whole.
        (_, ["api", "observe", "v1", rest @ ..]) => {
            let rest: Vec<String> = rest.iter().map(|s| (*s).to_owned()).collect();
            let q = q.clone();
            crate::rt::spawn(async move {
                let rest: Vec<&str> = rest.iter().map(String::as_str).collect();
                crate::observe::api::route(&rest, &q, req).await
            })
            .await
            .unwrap_or_else(|_| Err((StatusCode::INTERNAL_SERVER_ERROR, "the request's task ended before it answered".to_owned())))
            .map_err(|(c, m)| err(c, m))
        }
        // W14: diffs and stacks.
        (_, ["api", "diffs" | "change-requests" | "stacks" | "comments" | "review" | "queue", ..]) | (&Method::GET, ["api", "repos", _, "diffs" | "queue" | "change-requests"] | ["api", "repos", _, "lands", _]) | (&Method::GET | &Method::PUT, ["api", "repos", _, "agent-fixes" | "ladder"]) | (&Method::POST, ["api", "repos", _, "change-requests" | "submit" | "about"]) => {
            let segs: Vec<&str> = s.iter().skip(1).copied().collect();
            let body: serde_json::Value = if matches!(method, Method::POST | Method::PATCH | Method::PUT) { boxed(|| srv::body_json(req.into_body(), JSON_LIMIT)).await.unwrap_or(serde_json::Value::Null) } else { serde_json::Value::Null };
            boxed(|| crate::ci::diffs::route(&hub, &method, &segs, &q, &caller, &body)).await.map_err(|(c, m)| err(c, m))
        }
        (&Method::GET, ["api", "overview"]) => boxed(|| overview(&hub, &caller)).await,
        (&Method::GET, ["api", "me", "notifications"]) => boxed(|| crate::ci::notify::mine(&hub, &caller, None)).await.map_err(|(s, m)| ApiError(s, m)),
        (&Method::PUT, ["api", "me", "notifications"]) => {
            let body: serde_json::Value = boxed(|| srv::body_json(req.into_body(), 1 << 20)).await.map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            boxed(|| crate::ci::notify::mine(&hub, &caller, Some(body))).await.map_err(|(s, m)| ApiError(s, m))
        }
        // docs/deploy-api.md §12.3: one's own chat accounts.
        (_, ["api", "me", "chat", rest @ ..]) => {
            let rest: Vec<&str> = rest.to_vec();
            boxed(|| crate::ci::chat::mine(&hub, &caller, &method, &rest)).await.map_err(|(s, m)| ApiError(s, m))
        }
        // The HTTP/3 door's URL, pinned hashes and a single-use ticket for the caller (goal 4 step 6).
        (&Method::POST, ["api", "me", "door"]) => Ok(srv::json(StatusCode::OK, &crate::h3door::info(&caller.name))),
        // W10: one's own passkeys and password.
        (&Method::GET, ["api", "me", "passkeys"]) => boxed(|| crate::authz::passkeys::list(&hub, &caller)).await.map(|v| srv::json(StatusCode::OK, &v)).map_err(|(s, m)| ApiError(s, m)),
        (&Method::POST, ["api", "me", "passkeys", "options"]) => {
            let origin = req.headers().get(header::ORIGIN).and_then(|v| v.to_str().ok()).map(str::to_owned);
            boxed(|| crate::authz::passkeys::register_options(&hub, &caller, origin.as_deref())).await.map(|v| srv::json(StatusCode::OK, &v)).map_err(|(s, m)| ApiError(s, m))
        }
        (&Method::POST, ["api", "me", "passkeys"]) => {
            let body: serde_json::Value = boxed(|| srv::body_json(req.into_body(), 1 << 16)).await.map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            boxed(|| crate::authz::passkeys::register(&hub, &caller, &body)).await.map(|v| srv::json(StatusCode::CREATED, &v)).map_err(|(s, m)| ApiError(s, m))
        }
        (&Method::DELETE, ["api", "me", "passkeys", id]) => boxed(|| crate::authz::passkeys::remove(&hub, &caller, id)).await.map(|()| srv::status(StatusCode::NO_CONTENT)).map_err(|(s, m)| ApiError(s, m)),
        (&Method::GET, ["api", "me", "password"]) => boxed(|| crate::authz::passwords::get(&hub, &caller)).await.map(|v| srv::json(StatusCode::OK, &v)).map_err(|(s, m)| ApiError(s, m)),
        (&Method::PUT, ["api", "me", "password"]) => {
            let body: serde_json::Value = boxed(|| srv::body_json(req.into_body(), 1 << 16)).await.map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            boxed(|| crate::authz::passwords::set(&hub, &caller, &body)).await.map(|v| srv::json(StatusCode::OK, &v)).map_err(|(s, m)| ApiError(s, m))
        }
        (&Method::GET, ["api", "system"]) => Ok(boxed(|| system(&hub)).await),
        (&Method::GET, ["api", "audit"]) => boxed(|| audit(&hub, &q)).await,
        (&Method::GET, ["api", "updates"]) => Ok(boxed(|| updates_overview(&hub, false)).await),
        (&Method::POST, ["api", "updates", "check"]) => Ok(boxed(|| updates_overview(&hub, true)).await),
        (&Method::GET, ["api", "updates", "runners"]) => Ok(srv::json(StatusCode::OK, &boxed(|| crate::doctor::updates_for(&hub, &caller)).await)),
        (&Method::POST, ["api", "updates", "runners"]) => Ok(boxed(|| upgrade_all_runners(&hub)).await),
        (&Method::POST, ["api", "updates", "build"]) => boxed(|| build_every_platform(&hub)).await,
        (&Method::POST, ["api", "updates", "runners", name]) => boxed(|| upgrade_runner(&hub, name, req)).await,
        (&Method::PUT, ["api", "updates", "runners", name]) => boxed(|| set_runner_policy(&hub, name, req)).await,
        (&Method::PUT, ["api", "updates", "policy"]) => boxed(|| set_policies(&hub, req)).await,
        (&Method::PUT, ["api", "updates", "waits"]) => boxed(|| set_waits(&hub, req)).await,
        (&Method::DELETE, ["api", "updates", "waits"]) => boxed(|| reset_waits(&hub)).await,
        (&Method::GET, ["api", "updates", "skew"]) => Ok(srv::json(StatusCode::OK, &boxed(|| skew_view(&hub)).await)),
        (&Method::PUT, ["api", "updates", "skew"]) => boxed(|| set_skew(&hub, req)).await,
        (&Method::DELETE, ["api", "updates", "skew"]) => boxed(|| reset_skew(&hub)).await,
        (&Method::POST, ["api", "updates", "server"]) => boxed(|| upgrade_server(&hub, req)).await,
        (&Method::POST, ["api", "updates", "binaries"]) => boxed(|| upload_binary(&hub, &q, req)).await,
        (&Method::DELETE, ["api", "updates", "binaries", target]) => boxed(|| delete_binary(&hub, target)).await,
        (&Method::GET, ["api", "updates", "jobs", id]) => match hub.updates.job(id) {
            Some(j) => boxed(|| update_job(&hub, j)).await,
            None => Err(err(StatusCode::NOT_FOUND, "no such upgrade")),
        },
        (&Method::GET, ["api", "metrics", "history"]) => Ok(history(&hub, &q)),
        (&Method::GET, ["api", "repos"]) => {
            let listed = boxed(|| crate::reads::repos(&hub.store, |r| caller.can("runs.read", &crate::authz::Scope::Repo { repo: r.name.clone() }))).await?;
            Ok(srv::json(StatusCode::OK, &listed))
        }
        (&Method::POST, ["api", "repos"]) => boxed(|| add_repo(&hub, req)).await,
        (&Method::DELETE, ["api", "repos", id]) => boxed(|| delete_repo(&hub, id)).await,
        (&Method::POST, ["api", "repos", id, "poll"]) => boxed(|| poll_repo(&hub, id)).await,
        (&Method::POST, ["api", "repos", id, "trigger"]) => {
            let id = (*id).to_owned();
            boxed(|| trigger(&hub, &id, req)).await
        }
        (&Method::GET, ["api", "runs"]) => boxed(|| runs(&hub, &q, &caller)).await,
        (&Method::GET, ["api", "runs", id]) => boxed(|| run(&hub, id, &q)).await,
        (&Method::POST, ["api", "runs", id, "cancel"]) => {
            let id = (*id).to_owned();
            boxed(|| cancel(&hub, &id, req)).await
        }
        (&Method::POST, ["api", "runs", id, "retry"]) => {
            let id = (*id).to_owned();
            boxed(|| retry(&hub, &id, req)).await
        }
        (&Method::POST, ["api", "runs", id, verb @ ("pause" | "resume")]) => {
            let (id, paused) = ((*id).to_owned(), *verb == "pause");
            boxed(|| pause(&hub, &id, req, paused)).await
        }
        (&Method::GET, ["api", "jobs", id]) => boxed(|| job(&hub, id)).await,
        // What a job lacks, and an agent to install it (crate::toolfix).
        (&Method::GET, ["api", "jobs", id, "tools"]) => boxed(|| crate::toolfix::report(&hub, id)).await.map(|v| srv::json(StatusCode::OK, &v)).map_err(|(c, m)| err(c, m)),
        (&Method::POST, ["api", "jobs", id, "tools", "fix"]) => {
            let (id, ask) = ((*id).to_owned(), boxed(|| srv::body_json::<crate::toolfix::Ask>(req.into_body(), JSON_LIMIT)).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?);
            boxed(|| crate::toolfix::fix(Arc::clone(&hub), caller.clone(), &id, ask)).await.map(|v| srv::json(StatusCode::OK, &v)).map_err(|(c, m)| err(c, m))
        }
        (&Method::GET, ["api", "jobs", id, "logs"]) => boxed(|| job_logs(&hub, id, &q)).await,
        (&Method::GET, ["api", "jobs", id, "logs", "times"]) => boxed(|| job_log_times(&hub, id, &q)).await,
        (&Method::GET, ["api", "jobs", id, "logs", "ws"]) => {
            let (id, step) = ((*id).to_owned(), q.get("step").and_then(|v| v.parse().ok()).unwrap_or(0));
            job_logs_ws(hub.clone(), id, step, req)
        }
        (&Method::GET, ["api", "jobs", id, "artifacts"]) => boxed(|| get_artifacts(&hub, id)).await,
        (&Method::GET, ["api", "agents"]) => Ok(boxed(|| agents_read(&hub, &caller)).await),
        (&Method::GET, ["api", "reach", "runners"]) => {
            let want = srv::query(&req).get("want").cloned();
            reach_runners(&hub, want.as_deref())
        }
        // The copies of runners' trees other runners keep (docs/reach.md §5.1): the rules for them,
        // what each runner holds and let go, and asking runners to let go now.
        (&Method::GET, ["api", "reach", "keep"]) => Ok(srv::json(StatusCode::OK, &boxed(|| hub.copies.view(&hub)).await)),
        (m @ (&Method::PUT | &Method::DELETE), ["api", "reach", "keep", "rules"]) => boxed(|| keep_rules(&hub, &caller, crate::reach::copies::Whose::System, *m == Method::PUT, req)).await,
        (m @ (&Method::PUT | &Method::DELETE), ["api", "reach", "keep", "runners", name]) => boxed(|| keep_rules(&hub, &caller, crate::reach::copies::Whose::Runner(name), *m == Method::PUT, req)).await,
        (m @ (&Method::PUT | &Method::DELETE), ["api", "reach", "keep", "repos", key @ ..]) if !key.is_empty() => {
            let key = key.join("/");
            boxed(|| keep_rules(&hub, &caller, crate::reach::copies::Whose::Repo(&key), *m == Method::PUT, req)).await
        }
        (&Method::POST, ["api", "reach", "keep", "now"]) => boxed(|| keep_now(&hub, &caller, None)).await,
        (&Method::POST, ["api", "reach", "keep", "runners", name, "now"]) => boxed(|| keep_now(&hub, &caller, Some(name))).await,
        (&Method::GET, ["api", "agents", "join"]) => Ok(join_info(&hub)),
        (&Method::POST, ["api", "outposts", "invite"]) => Ok(outpost_invite(&hub)),
        (&Method::GET, ["api", "discovery"]) => Ok(boxed(|| discovery(&hub)).await),
        (&Method::POST, ["api", "discovery", "scan"]) => boxed(|| discovery_scan(&hub, req)).await,
        (&Method::POST, ["api", "discovery", "probe"]) => boxed(|| discovery_probe(&hub, req)).await,
        (&Method::POST, ["api", "discovery", "invite"]) => boxed(|| discovery_invite(&hub, req)).await,
        (&Method::DELETE, ["api", "discovery", "invite", id]) => Ok(boxed(|| discovery_uninvite(&hub, id)).await),
        (&Method::GET, ["api", "join-requests"]) => Ok(srv::json(StatusCode::OK, &hub.joins.list(Instant::now(), &hub.pki.ca_der))),
        (&Method::POST, ["api", "join-requests", id, "approve"]) => {
            let id = (*id).to_owned();
            let by = caller_of(&hub, req.headers()).await.map_or_else(|| "admin".to_owned(), |c| c.name);
            boxed(|| approve_join(&hub, &id, req, &by)).await
        }
        (&Method::POST, ["api", "join-requests", id, "deny"]) => boxed(|| deny_join(&hub, id)).await,
        (&Method::DELETE, ["api", "agents", name]) => {
            let by = caller_of(&hub, req.headers()).await.map_or_else(|| "admin".to_owned(), |c| c.name);
            boxed(|| forget_agent(&hub, name, &by)).await
        }
        // Identities kiln's CA issued: their revocations, and revoking one (W27-R01).
        (&Method::GET, ["api", "identities", "revocations"]) => boxed(|| identity_revocations(&hub)).await,
        // A person's short-lived certificate for a key made on their device (W27-R02).
        (&Method::POST, ["api", "me", "identity"]) => {
            let caller = caller_of(&hub, req.headers()).await.ok_or_else(|| err(StatusCode::UNAUTHORIZED, "sign in first"))?;
            boxed(|| me_identity(&hub, caller, req)).await
        }
        // The SSH keys a person signs commits with, bound to their identity (W27-R03).
        (&Method::GET | &Method::POST | &Method::DELETE, ["api", "me", "ssh-signers"]) => {
            let caller = caller_of(&hub, req.headers()).await.ok_or_else(|| err(StatusCode::UNAUTHORIZED, "sign in first"))?;
            boxed(|| me_ssh_signers(&hub, caller, req)).await
        }
        (&Method::GET, ["api", "identities"]) => {
            let kind = srv::query(&req).remove("kind");
            boxed(|| async {
                let list = crate::identities::list(&hub, kind.as_deref()).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
                Ok(srv::json(StatusCode::OK, &json!({ "identities": list })))
            })
            .await
        }
        // The trail of every signed statement kiln takes, a page of it at a time, and kiln's
        // checkpoints of it, one signed now when asked (W27-R09).
        (&Method::GET, ["api", "trail"]) => {
            let q = srv::query(&req);
            let number = |k: &str, or: u64| q.get(k).and_then(|v| v.parse().ok()).unwrap_or(or);
            let (from, limit) = (number("from", 0), number("limit", 100));
            boxed(|| async { crate::trail::page(&hub.store, from, limit).await.map(|v| srv::json(StatusCode::OK, &v)).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))) }).await
        }
        (&Method::GET, ["api", "trail", "checkpoints"]) => {
            let limit = srv::query(&req).get("limit").and_then(|v| v.parse().ok()).unwrap_or(10);
            boxed(|| async { crate::trail::checkpoints_page(&hub.store, limit).await.map(|v| srv::json(StatusCode::OK, &v)).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))) }).await
        }
        (&Method::GET, ["api", "trail", "anchors"]) => {
            boxed(|| async { crate::trail::anchors(&hub).await.map(|v| srv::json(StatusCode::OK, &v)).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))) }).await
        }
        // The trail as a tiled log (C2SP tlog-tiles): its checkpoint, its tiles and its entries.
        (&Method::GET, ["api", "trail", "log", rest @ ..]) => {
            let path = rest.join("/");
            boxed(|| async {
                match crate::trail::log(&hub.store, &path).await {
                    Ok(Some((bytes, ctype, immutable))) => {
                        let r = srv::response(StatusCode::OK, ctype, srv::full(bytes::Bytes::from(bytes)));
                        Ok(srv::with_header(r, header::CACHE_CONTROL, if immutable { "private, max-age=31536000, immutable" } else { "private, max-age=5" }))
                    }
                    Ok(None) => Err(err(StatusCode::NOT_FOUND, format!("the trail has no {path}"))),
                    Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))),
                }
            })
            .await
        }
        (&Method::POST, ["api", "trail", "checkpoints"]) => boxed(|| async {
            let size = crate::trail::checkpoint(&hub, true).await.map_err(|e| err(StatusCode::CONFLICT, format!("the trail's checkpoint isn't signed: {e:#}")))?;
            Ok(srv::json(StatusCode::OK, &json!({ "size": size })))
        })
        .await,
        // kiln verify (W27-R10): the chain back from a deploy, an artifact, a commit or a change,
        // each link with whether it holds, the first that doesn't named.
        (&Method::GET, ["api", "verify"]) => {
            let q = srv::query(&req);
            boxed(|| async {
                let target = crate::verify::Target::of(&q).map_err(|(s, e)| err(s, e))?;
                crate::verify::verify(&hub, &target).await.map(|v| srv::json(StatusCode::OK, &v)).map_err(|(s, e)| err(s, e))
            })
            .await
        }
        // The same walk for an auditor, to check offline against kiln's root; and a revision's
        // Source VSA (SLSA's source track), signed as kiln's server and kept in the trail.
        (&Method::GET, ["api", "verify", "export"]) => {
            let q = srv::query(&req);
            boxed(|| async {
                let target = crate::verify::Target::of(&q).map_err(|(s, e)| err(s, e))?;
                crate::verify::export(&hub, &target).await.map(|v| srv::json(StatusCode::OK, &v)).map_err(|(s, e)| err(s, e))
            })
            .await
        }
        (&Method::POST, ["api", "verify", "vsa"]) => boxed(|| async {
            let body: serde_json::Value = boxed(|| srv::body_json(req.into_body(), JSON_LIMIT)).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
            let asked = body.get("commit").and_then(serde_json::Value::as_str).unwrap_or_default();
            let crate::verify::Target::Commit { repo, sha } = crate::verify::Target::parse("commit", asked).map_err(|e| err(StatusCode::BAD_REQUEST, e))? else {
                return Err(err(StatusCode::BAD_REQUEST, "`commit`: <repository>@<commit>"));
            };
            crate::verify::vsa(&hub, &repo, &sha).await.map(|v| srv::json(StatusCode::OK, &v)).map_err(|(s, e)| err(s, e))
        })
        .await,
        // Custody of what changes hands (W27-R08): a signed record taken, a thing's chain walked
        // now, the breaks kiln found and what it refused.
        (&Method::POST, ["api", "custody"]) => {
            let by = caller_of(&hub, req.headers()).await.map_or_else(|| "admin".to_owned(), |c| c.name);
            boxed(|| custody_take(&hub, req, &by)).await
        }
        // Walked for whoever audits it: each hop in words; `export=true`, every record it read as
        // signed, kiln's CA certificate and its revocations (the Transfers plan's step 15).
        (&Method::GET, ["api", "custody", "walk"]) => {
            let mut q = srv::query(&req);
            let (digest, name, export) = (q.remove("digest").unwrap_or_default(), q.remove("name"), q.remove("export").is_some_and(|e| e == "true" || e == "1"));
            boxed(|| async {
                let digest = match name {
                    Some(n) => crate::custody::head_named(&hub.store, &n).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?.unwrap_or(n),
                    None => digest,
                };
                let walked = crate::custody::walk(&hub.store, &hub.pki, &digest, crate::util::now_unix(), export).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
                walked.map(|v| srv::json(StatusCode::OK, &v)).ok_or_else(|| err(StatusCode::NOT_FOUND, format!("kiln has no custody record of {digest:?}")))
            })
            .await
        }
        (&Method::GET, ["api", "custody", "breaks"]) => boxed(|| async {
            let breaks = crate::custody::breaks(&hub.store, crate::custody::LIST_MAX).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
            Ok(srv::json(StatusCode::OK, &json!({ "breaks": breaks })))
        })
        .await,
        // The relays' streams (W27-R08's relays), and their records no receipt answered `lapse`
        // seconds on (kiln's own lapse unless said); posted, those are recorded as breaks naming
        // their relays now, as kiln does once they lapse.
        (&Method::GET | &Method::POST, ["api", "custody", "relays"]) => {
            let lapse = srv::query(&req).remove("lapse").and_then(|l| l.parse::<i64>().ok()).unwrap_or(crate::custody::LAPSE_AFTER);
            let post = req.method() == Method::POST;
            boxed(|| async {
                let at = crate::util::now_unix();
                let v = if post {
                    json!({ "recorded": crate::relays::unanswered(&hub.store, lapse, at, true).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))? })
                } else {
                    crate::relays::view(&hub.store, lapse, at).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
                };
                Ok(srv::json(StatusCode::OK, &v))
            })
            .await
        }
        (&Method::GET, ["api", "custody"]) => {
            let mut q = srv::query(&req);
            let (digest, name) = (q.remove("digest").unwrap_or_default(), q.remove("name"));
            boxed(|| async {
                // By a thing's name: the head of its newest chain.
                let digest = match name {
                    Some(n) => crate::custody::head_named(&hub.store, &n).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?.unwrap_or(n),
                    None => digest,
                };
                let chain = crate::custody::chain(&hub.store, &hub.pki.ca_der, &hub.pki.organization(), &digest, crate::util::now_unix()).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
                chain.map(|v| srv::json(StatusCode::OK, &v)).ok_or_else(|| err(StatusCode::NOT_FOUND, format!("kiln has no custody record of {digest:?}")))
            })
            .await
        }
        (&Method::GET, ["api", "transfers", "refused"]) => boxed(|| async {
            let refused = crate::custody::refusals(&hub.store, crate::custody::LIST_MAX).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
            Ok(srv::json(StatusCode::OK, &json!({ "refused": refused })))
        })
        .await,
        // Agent sessions' hand-overs that went unsigned, newest first (docs/ci.md W27-R08).
        (&Method::GET, ["api", "transfers", "unsigned"]) => boxed(|| async {
            let unsigned = crate::custody::unsigned_list(&hub.store, crate::custody::LIST_MAX).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
            Ok(srv::json(StatusCode::OK, &json!({ "unsigned": unsigned })))
        })
        .await,
        (&Method::POST, ["api", "identities", "revoke"]) => {
            let by = caller_of(&hub, req.headers()).await.map_or_else(|| "admin".to_owned(), |c| c.name);
            boxed(|| identity_revoke(&hub, req, &by)).await
        }
        // What's wrong with a runner, and repairs to it (`crate::doctor`, docs/releases.md).
        (&Method::GET, ["api", "agents", name, "diagnose"]) => boxed(|| crate::doctor::diagnose(&hub, name)).await.map(|v| srv::json(StatusCode::OK, &v)).map_err(|(c, m)| err(c, m)),
        (&Method::POST, ["api", "agents", name, "repair"]) => {
            let name = (*name).to_owned();
            let ask: crate::doctor::Ask = boxed(|| srv::body_json(req.into_body(), JSON_LIMIT)).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
            boxed(|| crate::doctor::repair(&hub, &name, &ask)).await.map(|v| srv::json(StatusCode::OK, &v)).map_err(|(c, m)| err(c, m))
        }
        (&Method::POST, ["api", "agents", name, "drain"]) => {
            let name = (*name).to_owned();
            boxed(|| drain_agent(&hub, &name, req)).await
        }
        (&Method::POST, ["api", "agents", name, "label"]) => {
            let name = (*name).to_owned();
            boxed(|| label_agent(&hub, &name, req)).await
        }
        (&Method::POST, ["api", "agents", name, "probe"]) => boxed(|| probe_agent(&hub, name)).await.map_err(|(c, m)| err(c, m)),
        (&Method::GET, ["api", "front-doors"]) => front_door(boxed(|| crate::frontdoor::control::list(&hub)).await),
        (&Method::GET, ["api", "headroom"]) => front_door(boxed(|| crate::headroom::control::list(&hub)).await),
        (&Method::GET, ["api", "runners", name, "headroom"]) => front_door(boxed(|| crate::headroom::control::get(&hub, name)).await),
        (&Method::PUT, ["api", "runners", name, "headroom"]) => {
            let name = (*name).to_owned();
            front_door(boxed(|| crate::headroom::control::put(&hub, &name, req)).await)
        }
        (&Method::GET, ["api", "runners", name, "front-door"]) => front_door(boxed(|| crate::frontdoor::control::get(&hub, name)).await),
        (&Method::PUT, ["api", "runners", name, "front-door"]) => {
            let name = (*name).to_owned();
            front_door(boxed(|| crate::frontdoor::control::put(&hub, &name, req)).await)
        }
        (&Method::POST, ["api", "runners", name, "front-door", "cert"]) => {
            let name = (*name).to_owned();
            front_door(boxed(|| crate::frontdoor::control::upload(&hub, &name, req)).await)
        }
        (&Method::GET, ["api", "runners", name, "front-door", "cert"]) => front_door(boxed(|| crate::frontdoor::control::download(&hub, name)).await),
        // How many agent sessions a machine runs at once (`crate::harness::limit`).
        (&Method::GET, ["api", "runners", name, "agent-limit"]) => boxed(|| crate::harness::limit::get(&hub, name)).await.map_err(|(c, m)| err(c, m)),
        (&Method::PUT, ["api", "runners", name, "agent-limit"]) => {
            let name = (*name).to_owned();
            boxed(|| crate::harness::limit::put(&hub, &name, req)).await.map_err(|(c, m)| err(c, m))
        }
        (&Method::POST, ["api", "runners", name, "front-door", "cert", "renew"]) => front_door(boxed(|| crate::frontdoor::control::renew(&hub, name)).await),
        (&Method::GET, ["api", "tailscale"]) => front_door(boxed(|| crate::frontdoor::tailnet::get(&hub)).await),
        (&Method::PUT, ["api", "tailscale"]) => front_door(boxed(|| crate::frontdoor::tailnet::put(&hub, req)).await),
        (&Method::DELETE, ["api", "tailscale"]) => front_door(boxed(|| crate::frontdoor::tailnet::delete(&hub)).await),
        (&Method::PUT, ["api", "settings", "password"]) => boxed(|| set_password(&hub, req)).await,
        (&Method::DELETE, ["api", "settings", "password"]) => boxed(|| clear_password(&hub)).await,
        // The browser protocol over one WebSocket (goal 3 of the browser protocol program, `crate::live`):
        // a ticket for the open, the socket, and the caller's own sockets as the server holds them.
        (&Method::POST, ["api", "live", "ticket"]) => Ok(crate::live::ticket(&hub, &caller, &req)),
        (&Method::GET, ["api", "live"]) => Ok(crate::live::socket(hub.clone(), caller.clone(), req)),
        (&Method::GET, ["api", "live", "sockets"]) => Ok(crate::live::sockets(&hub, &caller)),
        (&Method::GET, ["api", "events"]) => {
            // A person of some repositories hears theirs (W10); the server's scope hears it all.
            let narrow = boxed(|| crate::authz::gate::readable(&hub, &caller, "runs.read"))
                .await
                .map(|repos| Narrow { repos: repos.into_iter().collect(), person: crate::ci::inbox::person(&crate::ci::review::who(&caller).0), runs: HashMap::new() });
            // And of agents, the ones it sees (docs/agents.md, "Whose agents").
            let everyone = caller.agent.is_none() && (caller.is_owner() || caller.can("agents.manage", &crate::authz::Scope::Server));
            let agents = (!everyone).then(|| AgentsSeen { caller: caller.clone(), seen: HashMap::new() });
            events_ws(hub.clone(), req, Asked::of(&q), narrow, agents)
        }
        (&Method::GET, ["api", "ai-sessions"]) => Ok(ai_sessions(&hub, &caller)),
        (&Method::GET, ["api", "ai-sessions", "silent"]) => Ok(ai_silent(&hub, &caller)),
        (&Method::GET, ["api", "ai-sessions", machine, pid, "transcript"]) => {
            may_run_on(&caller, &format!("agent:{machine}"), "read the sessions found on that machine")?;
            boxed(|| ai_transcript(&hub, machine, pid)).await
        }
        (&Method::GET, ["api", "ai-sessions", machine, pid, "view"]) => {
            may_run_on(&caller, &format!("agent:{machine}"), "read the sessions found on that machine")?;
            boxed(|| ai_view(&hub, machine, pid)).await
        }
        (&Method::POST, ["api", "ai-sessions", machine, pid, "takeover"]) => {
            may_run_on(&caller, &format!("agent:{machine}"), "take over a session found on that machine")?;
            boxed(|| ai_takeover(&hub, &caller, machine, pid, &q, req)).await
        }
        (&Method::POST, ["api", "ai-sessions", machine, pid, "fork"]) => {
            may_run_on(&caller, &format!("agent:{machine}"), "branch a session found on that machine")?;
            boxed(|| ai_fork(&hub, &caller, machine, pid)).await
        }
        (&Method::GET, ["api", "sync"]) => Ok(srv::json(StatusCode::OK, &hub.sync.view())),
        (&Method::GET, ["api", "sync", "activity"]) => Ok(sync_activity(&hub, &req)),
        (&Method::POST, ["api", "sync"]) => boxed(|| sync_create(&hub, req)).await,
        (&Method::DELETE, ["api", "sync", id]) => boxed(|| sync_ask(&hub, "sync.delete", id, crate::sync::server::Ask::Delete { repo: (*id).to_owned() })).await,
        (&Method::POST, ["api", "sync", id, "copies"]) => boxed(|| sync_add_copy(&hub, id, req)).await,
        (&Method::DELETE, ["api", "sync", id, "copies", member]) => {
            let member: u64 = member.parse().map_err(|_| err(StatusCode::BAD_REQUEST, "bad member"))?;
            boxed(|| sync_ask(&hub, "sync.copy.remove", id, crate::sync::server::Ask::RemoveCopy { repo: (*id).to_owned(), member })).await
        }
        (&Method::POST, ["api", "sync", id, "conflicts", "resolve"]) => boxed(|| sync_resolve(&hub, id, req)).await,
        (&Method::GET, ["api", "places"]) => Ok(srv::json(StatusCode::OK, &boxed(|| crate::sync::places::now(&hub)).await?)),
        (&Method::POST, ["api", "places", "look"]) => Ok(boxed(|| places_look(&hub)).await),
        (&Method::POST, ["api", "places", kind @ ("clone" | "sync" | "move" | "remove")]) => {
            let kind = (*kind).to_owned();
            boxed(|| places_start(&hub, &kind, req)).await
        }
        (&Method::POST, ["api", "places", "tasks", id, "retry"]) => {
            let tasks = boxed(|| crate::sync::tasks::retry(&hub, id)).await.map_err(|e| err(StatusCode::CONFLICT, e))?;
            boxed(|| hub.store.audit("admin".into(), "places.retry".into(), (*id).to_owned())).await;
            Ok(srv::json(StatusCode::CREATED, &tasks))
        }
        (&Method::DELETE, ["api", "places", "tasks", id]) => {
            if hub.tasks.dismiss(id) {
                hub.emit(&crate::wire::events::ServerEvent::Places);
                Ok(srv::status(StatusCode::NO_CONTENT))
            } else {
                Err(err(StatusCode::CONFLICT, "no such task, or it is still under way"))
            }
        }
        // Agents attached to what they look after, and their incidents (docs/ci.md W20-R08 to R10).
        (&Method::GET, ["api", "attachments"]) => boxed(|| own_op(&hub, &caller, "attachments.list", query_value(&q))).await,
        (&Method::POST, ["api", "attachments", "pause"]) => {
            let body = body_value(req).await?;
            boxed(|| own_op(&hub, &caller, "attachments.pause", body)).await
        }
        (&Method::POST, ["api", "attachments"]) => {
            let body = body_value(req).await?;
            boxed(|| own_op(&hub, &caller, "attachments.save", body)).await
        }
        (&Method::GET, ["api", "attachments", a]) => boxed(|| own_op(&hub, &caller, "attachments.get", json!({ "attachment": a }))).await,
        (&Method::PUT, ["api", "attachments", a]) => {
            let a = (*a).to_owned();
            let mut body = body_value(req).await?;
            if let Some(o) = body.as_object_mut() {
                o.insert("attachment".into(), json!(a));
            }
            boxed(|| own_op(&hub, &caller, "attachments.save", body)).await
        }
        (&Method::DELETE, ["api", "attachments", a]) => boxed(|| own_op(&hub, &caller, "attachments.remove", json!({ "attachment": a }))).await,
        (&Method::GET, ["api", "incidents"]) => boxed(|| own_op(&hub, &caller, "incidents.list", query_value(&q))).await,
        (&Method::GET, ["api", "incidents", i]) => boxed(|| own_op(&hub, &caller, "incidents.get", json!({ "incident": i }))).await,
        (&Method::POST, ["api", "incidents", i, "report"]) => {
            let i = (*i).to_owned();
            let mut body = body_value(req).await?;
            if let Some(o) = body.as_object_mut() {
                o.insert("incident".into(), json!(i));
            }
            boxed(|| own_op(&hub, &caller, "incidents.report", body)).await
        }
        // A plan run as one agent per step (docs/plans.md): its own page's routes.
        // Research (docs/research.md): each operation is its route.
        (&Method::GET, ["api", "research"]) => boxed(|| own_op(&hub, &caller, "research.list", query_value(&q))).await,
        (&Method::POST, ["api", "research"]) => {
            let body = boxed(|| body_value(req)).await?;
            boxed(|| own_op(&hub, &caller, "research.start", body)).await
        }
        (&Method::GET, ["api", "research", id]) => {
            let mut input = query_value(&q);
            if let Some(o) = input.as_object_mut() {
                o.insert("id".into(), json!(id));
            }
            boxed(|| own_op(&hub, &caller, "research.get", input)).await
        }
        (&Method::POST, ["api", "research", id, "findings"]) => {
            let id = (*id).to_owned();
            let mut body = boxed(|| body_value(req)).await?;
            if let Some(o) = body.as_object_mut() {
                o.insert("id".into(), json!(id));
            }
            boxed(|| own_op(&hub, &caller, "research.add", body)).await
        }
        (&Method::GET, ["api", "harness", "conversations", "search"]) => boxed(|| own_op(&hub, &caller, "conversations.search", query_value(&q))).await,
        (&Method::GET, ["api", "plans", "runs"]) => boxed(|| own_op(&hub, &caller, "plans.runs.list", query_value(&q))).await,
        (&Method::GET, ["api", "plans", "runs", run]) => boxed(|| own_op(&hub, &caller, "plans.runs.get", json!({ "run": run }))).await,
        (&Method::POST, ["api", "plans", "runs", run, "stop"]) => boxed(|| own_op(&hub, &caller, "plans.runs.stop", json!({ "run": run }))).await,
        (&Method::POST, ["api", "plans", "runs", run, "steps", n, "retry"]) => {
            // With the try it replaces, when the body says it (CP-T3.2).
            let (run, n) = ((*run).to_owned(), (*n).to_owned());
            let mut body = boxed(|| body_value(req)).await?;
            if let Some(o) = body.as_object_mut() {
                o.insert("run".into(), json!(run));
                o.insert("step".into(), json!(n));
            }
            boxed(|| own_op(&hub, &caller, "plans.runs.retry", body)).await
        }
        // Taking a step over, and saying it's met, are a person's: no operation does them.
        (&Method::POST, ["api", "plans", "runs", run, "steps", n, "met"]) => {
            let n: usize = n.parse().map_err(|_| err(StatusCode::BAD_REQUEST, "a step's number, from 1"))?;
            let by = caller.name.clone();
            boxed(|| crate::plans::agents::met(&hub, run, n, &by)).await.map(|v| srv::json(StatusCode::OK, &v)).map_err(plan_err)
        }
        // Taken over, the step goes to the person the caller is, signed (docs/ci.md W27-R08): their
        // receipt is theirs when they say they sign it on their device (`signs`, then `receipt`),
        // else kiln's for them.
        (&Method::POST, ["api", "plans", "runs", run, "steps", n, "take"]) => {
            let n: usize = n.parse().map_err(|_| err(StatusCode::BAD_REQUEST, "a step's number, from 1"))?;
            let by = caller.name.clone();
            let signs = boxed(|| body_value(req)).await?.get("signs").and_then(serde_json::Value::as_bool).unwrap_or(false);
            let person = crate::identities::person_identity(&hub.store, &hub.pki.organization(), &caller).await.ok();
            boxed(|| crate::plans::agents::take(&hub, run, n, &by, person.as_deref(), signs)).await.map(|v| srv::json(StatusCode::OK, &v)).map_err(plan_err)
        }
        (&Method::POST, ["api", "plans", "runs", run, "steps", n, "receipt"]) => {
            let n: usize = n.parse().map_err(|_| err(StatusCode::BAD_REQUEST, "a step's number, from 1"))?;
            let person = crate::identities::person_identity(&hub.store, &hub.pki.organization(), &caller).await.map_err(|e| err(StatusCode::FORBIDDEN, e.to_string()))?;
            let body = boxed(|| body_value(req)).await?;
            // A browser that can't sign says why (`unsigned`): kiln's receipt for them, saying so.
            if let Some(missing) = body.get("unsigned").and_then(serde_json::Value::as_str) {
                let missing = missing.chars().take(300).collect::<String>();
                return boxed(|| crate::plans::agents::receipt_for(&hub, run, n, &person, &missing)).await.map(|v| srv::json(StatusCode::OK, &v)).map_err(plan_err);
            }
            let receipt = body.get("receipt").cloned().ok_or_else(|| err(StatusCode::BAD_REQUEST, "send your receipt: `receipt`, a Sigstore bundle signed on your device"))?;
            boxed(|| crate::plans::agents::receipt(&hub, run, n, &person, &receipt)).await.map(|v| srv::json(StatusCode::OK, &v)).map_err(plan_err)
        }
        // What a person signs to receive a step they took over, said as a verdict's is, so a
        // browser signs it with its own key (docs/ci.md W27-R08; ui/src/lib/signing.ts).
        (&Method::POST, ["api", "plans", "runs", run, "steps", n, "statement"]) => {
            let n: usize = n.parse().map_err(|_| err(StatusCode::BAD_REQUEST, "a step's number, from 1"))?;
            let person = crate::identities::person_identity(&hub.store, &hub.pki.organization(), &caller).await.map_err(|e| err(StatusCode::FORBIDDEN, e.to_string()))?;
            boxed(|| crate::plans::agents::receipt_statement(&hub, run, n, &person)).await.map(|v| srv::json(StatusCode::OK, &v)).map_err(plan_err)
        }
        (&Method::GET, ["api", "programs"]) => boxed(|| own_op(&hub, &caller, "programs.list", json!({}))).await,
        (&Method::GET, ["api", "programs", id]) => boxed(|| own_op(&hub, &caller, "programs.get", json!({"program": id}))).await,
        (&Method::PUT, ["api", "programs", id, "settings"]) => boxed(|| programs_op(&hub, &caller, "programs.set", json!({"program": id}), req)).await,
        (&Method::PUT, ["api", "programs", id, "tracks", track]) => boxed(|| programs_op(&hub, &caller, "programs.assign", json!({"program": id, "track": track}), req)).await,
        (&Method::GET, ["api", "plans"]) => boxed(|| plans_list(&hub, &q)).await,
        (&Method::POST, ["api", "plans"]) => boxed(|| plans_create(&hub, req)).await,
        (&Method::GET, ["api", "plans", id]) => boxed(|| crate::plans::get(&hub, id)).await.map(|p| srv::json(StatusCode::OK, &p)).map_err(plan_err),
        (&Method::PATCH, ["api", "plans", id]) => boxed(|| plans_update(&hub, id, req)).await,
        (&Method::DELETE, ["api", "plans", id]) => boxed(|| plans_delete(&hub, id)).await,
        (&Method::GET, ["api", "plans", id, "versions"]) => boxed(|| crate::plans::versions(&hub, id)).await.map(|v| srv::json(StatusCode::OK, &v)).map_err(plan_err),
        (&Method::GET, ["api", "plans", id, "runs"]) => boxed(|| crate::plans::runs(&hub, id)).await.map(|v| srv::json(StatusCode::OK, &v)).map_err(plan_err),
        (&Method::POST, ["api", "plans", id, "restore"]) => boxed(|| plans_restore(&hub, id, req)).await,
        (&Method::POST, ["api", "plans", id, "run"]) => {
            let id = (*id).to_owned();
            // Its idempotency key may come as the standard header (CP-T3.2).
            let key = idempotency_key(&req);
            let mut body = boxed(|| body_value(req)).await?;
            if let Some(o) = body.as_object_mut() {
                o.insert("id".into(), json!(id));
                if let Some(k) = key.filter(|_| !o.contains_key("key")) {
                    o.insert("key".into(), json!(k));
                }
            }
            boxed(|| own_op(&hub, &caller, "plans.run", body)).await
        }
        (&Method::POST, ["api", "plans", id, "share"]) => boxed(|| plans_share(&hub, id, req)).await,
        (&Method::POST, ["api", "plans", id, "review"]) => {
            let id = (*id).to_owned();
            // Its idempotency key may come as the standard header (CP-T3.2).
            let key = idempotency_key(&req);
            let mut body = boxed(|| body_value(req)).await?;
            if let Some(o) = body.as_object_mut() {
                o.insert("id".into(), json!(id));
                if let Some(k) = key.filter(|_| !o.contains_key("key")) {
                    o.insert("key".into(), json!(k));
                }
            }
            boxed(|| own_op(&hub, &caller, "plans.review", body)).await
        }
        (&Method::POST, ["api", "plans", id, "ready"]) => {
            let id = (*id).to_owned();
            let mut body = boxed(|| body_value(req)).await?;
            if let Some(o) = body.as_object_mut() {
                o.insert("id".into(), json!(id));
            }
            boxed(|| own_op(&hub, &caller, "plans.ready", body)).await
        }
        (&Method::GET, ["api", "voice"]) => {
            Ok(srv::json(StatusCode::OK, &serde_json::json!({ "transcribe": hub.cfg.voice.enabled(), "language": hub.cfg.voice.language })))
        }
        (&Method::POST, ["api", "voice", "transcribe"]) => boxed(|| voice_transcribe(&hub, &q, req)).await,
        (&Method::GET, ["api", "accounts"]) => boxed(|| accounts_status(&hub, &q)).await,
        (&Method::POST, ["api", "accounts", "login"]) => boxed(|| accounts_login(&hub, req)).await,
        (&Method::POST, ["api", "accounts", "logout"]) => boxed(|| accounts_logout(&hub, req)).await,
        (&Method::GET, ["api", "accounts", "flows", id]) => {
            crate::accounts::get(id).map(|f| srv::json(StatusCode::OK, &f)).ok_or_else(|| err(StatusCode::NOT_FOUND, "no such sign-in"))
        }
        (&Method::POST, ["api", "accounts", "flows", id, "input"]) => boxed(|| accounts_input(id, req)).await,
        (&Method::DELETE, ["api", "accounts", "flows", id]) => {
            crate::accounts::cancel(id).map(|()| srv::status(StatusCode::NO_CONTENT)).map_err(|e| err(StatusCode::NOT_FOUND, format!("{e:#}")))
        }
        (&Method::GET, ["api", "machines", target, "dirs"]) => boxed(|| machine_dirs(&hub, target, &q)).await,
        (&Method::GET, ["api", "terminal", "sessions"]) => Ok(srv::json(StatusCode::OK, &hub.terminals.visible())),
        (&Method::POST, ["api", "terminal", "sessions"]) => boxed(|| create_terminal(&hub, req)).await,
        (&Method::PUT, ["api", "terminal", "sessions", id, "name"]) => boxed(|| rename_terminal(&hub, id, req)).await,
        (&Method::DELETE, ["api", "terminal", "sessions", id]) => {
            if let Some(s) = hub.terminals.get(id) {
                s.close();
                boxed(|| hub.store.audit("admin".into(), "terminal.close".into(), s.target.clone())).await;
            }
            Ok(srv::status(StatusCode::NO_CONTENT))
        }
        (&Method::GET, ["api", "terminal", "ws"]) => boxed(|| terminal_ws(hub.clone(), q, req)).await,
        // Agents that run agents (`agents.*`, docs/agents.md): each operation is its route.
        (&Method::GET, ["api", "harness", "agents"]) => boxed(|| own_op(&hub, &caller, "agents.list", query_value(&q))).await,
        (&Method::POST, ["api", "harness", "agents"]) => {
            let body = boxed(|| body_value(req)).await?;
            boxed(|| own_op(&hub, &caller, "agents.start", body)).await
        }
        (&Method::POST, ["api", "harness", "agents", "wait"]) => {
            let body = boxed(|| body_value(req)).await?;
            boxed(|| own_op(&hub, &caller, "agents.wait", body)).await
        }
        // It takes delivery of the waiter's messages: a change, so not a GET (where an agent's id
        // would be read).
        (&Method::GET, ["api", "harness", "agents", "wait"]) => Err(err(StatusCode::METHOD_NOT_ALLOWED, "agents.wait takes delivery of your messages, so it's a POST: send its input as a JSON body")),
        (&Method::GET, ["api", "harness", "agents", id]) => boxed(|| own_op(&hub, &caller, "agents.get", json!({ "agent": id }))).await,
        (&Method::GET, ["api", "harness", "agents", id, what @ ("result" | "children")]) => {
            let op = format!("agents.{what}");
            boxed(|| own_op(&hub, &caller, &op, json!({ "agent": id }))).await
        }
        (&Method::GET, ["api", "harness", "changes"]) => boxed(|| own_op(&hub, &caller, "agents.changes", query_value(&q))).await,
        (&Method::GET, ["api", "harness", "agents", id, "workspace"]) => boxed(|| own_op(&hub, &caller, "agents.workspace", json!({ "agent": id }))).await,
        (&Method::GET, ["api", "harness", "agents", id, what @ ("commits" | "refs" | "compare")]) => {
            let mut v = query_value(&q);
            if let Some(o) = v.as_object_mut() {
                o.insert("agent".into(), json!(id));
            }
            let op = format!("agents.{what}");
            boxed(|| own_op(&hub, &caller, &op, v)).await
        }
        (&Method::POST, ["api", "harness", "agents", id, what @ ("send" | "interrupt" | "stop" | "change" | "land" | "about" | "credential")]) => {
            let (id, name) = ((*id).to_owned(), format!("agents.{what}"));
            // A send's idempotency key may come as the standard header (CP-T3.2).
            let key = idempotency_key(&req);
            let mut body = boxed(|| body_value(req)).await?;
            if let Some(o) = body.as_object_mut() {
                o.insert("agent".into(), json!(id));
                if let Some(k) = key.filter(|_| *what == "send" && !o.contains_key("key")) {
                    o.insert("key".into(), json!(k));
                }
            }
            boxed(|| own_op(&hub, &caller, &name, body)).await
        }
        (&Method::GET, ["api", "harness", "kinds"]) => Ok(harness_kinds(&hub, &q)),
        (&Method::GET, ["api", "harness", "workspaces"]) => Ok(boxed(|| harness_workspaces(&hub)).await),
        (&Method::GET, ["api", "harness", "sessions"]) => Ok(boxed(|| harness_sessions(&hub, &caller)).await),
        (&Method::POST, ["api", "harness", "sessions"]) => boxed(|| harness_open(&hub, req)).await,
        // As its owner encoded it once (goal 10, plan 01a1a87b2dae step 2).
        (&Method::GET, ["api", "harness", "sessions", id]) => match hub.harness.info_json(id) {
            Some(info) => Ok(pre_encoded(info)),
            None => Err(err(StatusCode::NOT_FOUND, "no such agent session")),
        },
        (&Method::DELETE, ["api", "harness", "sessions", id]) => {
            let id = (*id).to_owned();
            if boxed(|| hub.harness.close(&hub, &id)).await {
                let _ = boxed(|| crate::goals::forget(&hub, &id)).await;
                boxed(|| hub.copies.forget_agent(&hub, &id)).await;
                boxed(|| hub.store.audit("admin".into(), "agent.close".into(), id)).await;
                Ok(srv::status(StatusCode::NO_CONTENT))
            } else {
                Err(err(StatusCode::NOT_FOUND, "no such agent session"))
            }
        }
        (&Method::POST, ["api", "harness", "broadcast"]) => boxed(|| harness_broadcast(&hub, req)).await,
        (&Method::POST, ["api", "harness", "broadcast", "preview"]) => boxed(|| harness_broadcast_preview(&hub, req)).await,
        (&Method::POST, ["api", "harness", "machines", machine, "recover"]) => boxed(|| harness_recover(&hub, machine, req)).await,
        (&Method::POST, ["api", "harness", "sessions", id, "move"]) => {
            let body: serde_json::Value = boxed(|| srv::body_json(req.into_body(), JSON_LIMIT)).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
            let to = body.get("to").and_then(serde_json::Value::as_str).unwrap_or("").to_owned();
            let now = body.get("now").and_then(serde_json::Value::as_bool).unwrap_or(false);
            // Going on there in another harness, its conversation carried over (optional).
            let text = |k: &str| body.get(k).and_then(serde_json::Value::as_str).filter(|v| !v.is_empty()).map(str::to_owned);
            // Its machine away: from the server's copy of its conversation (`relocate::start`).
            match boxed(|| crate::harness::relocate::start(&hub, id, &to, now, text("kind"), text("model"))).await {
                Ok(v) => Ok(srv::json(StatusCode::ACCEPTED, &v)),
                Err(e) => Err(err(StatusCode::CONFLICT, e)),
            }
        }
        (&Method::POST, ["api", "harness", "sessions", id, "command"]) => {
            let id = (*id).to_owned();
            boxed(|| harness_command(&hub, &id, req)).await
        }
        // Its copies on other runners, and its own rules for them (docs/reach.md §5.1).
        (&Method::GET, ["api", "harness", "sessions", id, "keep"]) => Ok(srv::json(StatusCode::OK, &boxed(|| hub.copies.agent(&hub, id)).await)),
        (m @ (&Method::PUT | &Method::DELETE), ["api", "harness", "sessions", id, "keep"]) => boxed(|| keep_rules(&hub, &caller, crate::reach::copies::Whose::Agent(id), *m == Method::PUT, req)).await,
        (&Method::POST, ["api", "harness", "command"]) => boxed(|| harness_command_many(&hub, req)).await,
        (&Method::POST, ["api", "harness", "files"]) => boxed(|| harness_upload(&hub, &q, req)).await,
        (&Method::GET, ["api", "harness", "sessions", id, "ws"]) => harness_ws(hub.clone(), (*id).to_owned(), req),
        // A page it showed: its heights, as its machine and its viewers' own browsers measured them.
        (&Method::GET, ["api", "harness", "sessions", id, "pages", page, "heights"]) => boxed(|| page_heights(&hub, id, page)).await,
        (&Method::POST, ["api", "harness", "sessions", id, "pages", page, "heights"]) => boxed(|| page_seen(&hub, id, page, req)).await,
        // Pages streamed from its machine (`crate::harness::streams`).
        (&Method::GET, ["api", "harness", "sessions", id, "streams"]) => {
            let me = person(&req);
            Ok(srv::json(StatusCode::OK, &hub.harness.streams.of(id).iter().filter(|l| l.for_whom(me.as_deref())).map(|l| l.view()).collect::<Vec<_>>()))
        }
        (&Method::POST, ["api", "harness", "sessions", id, "streams"]) => {
            let id = (*id).to_owned();
            boxed(|| stream_open(&hub, &id, req)).await
        }
        (&Method::GET, ["api", "harness", "sessions", id, "streams", sid, "ws"]) => stream_ws(hub.clone(), id, sid, req),
        // Its apps opened on people's own devices (`crate::harness::opened`): theirs alone.
        (&Method::POST, ["api", "harness", "sessions", id, "opened"]) => {
            let id = (*id).to_owned();
            boxed(|| open_on_device(&hub, &id, req)).await
        }
        (&Method::GET, ["api", "harness", "sessions", id, "opened"]) => Ok(srv::json(StatusCode::OK, &hub.harness.opened.of(id, person(&req).as_deref()))),
        (&Method::DELETE, ["api", "harness", "sessions", id, "opened", oid]) => {
            if hub.harness.opened.end(id, oid, person(&req).as_deref()) {
                boxed(|| hub.store.audit("admin".into(), "agent.opened.end".into(), format!("{id}: {oid}"))).await;
                Ok(srv::status(StatusCode::NO_CONTENT))
            } else {
                Err(err(StatusCode::NOT_FOUND, "no such opening"))
            }
        }
        (&Method::GET, ["api", "harness", "sessions", id, "preview"]) => {
            let refusal = personal_refusal(&req);
            boxed(|| preview_look(&hub, id, "look", refusal)).await
        }
        (&Method::POST, ["api", "harness", "sessions", id, "preview", "stop"]) => boxed(|| preview_look(&hub, id, "stop", None)).await,
        (&Method::DELETE, ["api", "harness", "sessions", id, "streams", sid]) => {
            let me = person(&req);
            let ours = hub.harness.streams.get(sid).is_some_and(|l| l.session == *id && l.for_whom(me.as_deref()));
            if ours && boxed(|| hub.harness.streams.close(&hub, sid)).await {
                boxed(|| hub.store.audit("admin".into(), "agent.stream.close".into(), format!("{id}: {sid}"))).await;
                Ok(srv::status(StatusCode::NO_CONTENT))
            } else {
                Err(err(StatusCode::NOT_FOUND, "no such stream"))
            }
        }
        (&Method::GET, ["api", "harness", "sessions", id, "goals"]) => boxed(|| harness_goals(&hub, id)).await,
        (&Method::PUT, ["api", "harness", "sessions", id, "goals"]) => boxed(|| goals_set(&hub, &caller, id, req)).await,
        (&Method::POST, ["api", "harness", "sessions", id, "goals"]) => boxed(|| goals_add(&hub, &caller, id, req)).await,
        // Said met, split, one goal changed, or the stack started, paused (for a while, too) …: each
        // its operation (`goals.*`), the body its input.
        (&Method::POST, ["api", "harness", "sessions", id, "goals", what]) => {
            let (name, field) = match *what {
                "done" => ("goals.done", None),
                "order" => ("goals.order", None),
                "split" => ("goals.split", None),
                "plan" => ("goals.plan", None),
                "settings" => ("goals.settings", None),
                action => ("goals.control", Some(("action", (*action).to_owned()))),
            };
            // A control or an end asked for under its `Idempotency-Key` header: its key (CP-T3.2).
            let header = idempotency_key(&req).filter(|_| matches!(name, "goals.control" | "goals.done"));
            let mut body = boxed(|| body_value(req)).await?;
            if let Some(o) = body.as_object_mut() {
                o.insert("agent".into(), json!(id));
                if let Some((k, v)) = field {
                    o.insert(k.into(), json!(v));
                }
                if let Some(k) = header
                    && !o.contains_key("key")
                {
                    o.insert("key".into(), json!(k));
                }
            }
            boxed(|| own_op(&hub, &caller, name, body)).await
        }
        (&Method::PATCH, ["api", "harness", "sessions", id, "goals", goal]) => {
            let (id, goal) = ((*id).to_owned(), (*goal).to_owned());
            let mut body = boxed(|| body_value(req)).await?;
            if let Some(o) = body.as_object_mut() {
                o.insert("agent".into(), json!(id));
                o.insert("id".into(), json!(goal));
            }
            boxed(|| own_op(&hub, &caller, "goals.update", body)).await
        }
        (&Method::GET, ["api", "harness", "sessions", id, "terminals"]) => Ok(srv::json(StatusCode::OK, &crate::harness::screen::list(&hub, id))),
        (&Method::POST, ["api", "harness", "sessions", id, "terminals"]) => {
            let id = (*id).to_owned();
            boxed(|| harness_terminal(&hub, &id, req)).await
        }
        (&Method::GET, ["api", "harness", "history"]) => boxed(|| harness_history(&hub, &caller, &q)).await,
        (&Method::GET, ["api", "harness", "sessions", id, "events"]) => boxed(|| conversation_events(&hub, id, &q)).await,
        (&Method::GET, ["api", "harness", "sessions", id, "events", event]) => boxed(|| harness_event(&hub, id, event, &q)).await,
        (&Method::GET, ["api", "harness", "sessions", id, "timeline"]) => boxed(|| harness_timeline(&hub, id, &q)).await,
        (&Method::GET, ["api", "conversations", "search"]) => boxed(|| conversation_search(&hub, &q)).await,
        // The runners that keep a copy of the server's conversation store (docs/conversations.md, "Mirrors").
        (&Method::GET, ["api", "conversations", "mirrors"]) => Ok(srv::json(StatusCode::OK, &json!({ "mirrors": hub.harness.copies.as_ref().map(|m| m.view()).unwrap_or_default() }))),
        // Restoring the server's conversation store from a mirror's copy (admins: unlisted in authz).
        (&Method::GET, ["api", "conversations", "restore"]) => Ok(srv::json(StatusCode::OK, &hub.harness.restore_view())),
        (&Method::POST, ["api", "conversations", "restore"]) => boxed(|| conversation_restore(&hub, req)).await,
        // Every conversation store, as the server sees it, and one store's series (the System
        // page; docs/conversations.md, "Telemetry").
        (&Method::GET, ["api", "conversations", "status"]) => Ok(srv::json(StatusCode::OK, &crate::harness::stores::status(&hub))),
        (&Method::GET, ["api", "conversations", "series"]) => conversation_series(&hub, &q),
        (&Method::GET, ["api", "conversations", "log"]) => conversation_log(&hub, &q),
        // Files the conversation's messages mention: kept by the server, or read on its machine now.
        (&Method::GET, ["api", "harness", "sessions", id, "files", sha]) => boxed(|| harness_kept_file(&hub, id, sha, &req)).await,
        (&Method::GET, ["api", "harness", "sessions", id, "file"]) => boxed(|| harness_file(&hub, id, q.get("path").cloned(), &req)).await,
        // Recordings of its streams (`crate::browser::record`): theirs a person's own profile's.
        (&Method::GET, ["api", "harness", "sessions", id, "recordings"]) => Ok(srv::json(StatusCode::OK, &hub.harness.streams.recordings(id, person(&req).as_deref()))),
        (&Method::GET, ["api", "harness", "sessions", id, "recordings", rid, what]) => boxed(|| recording_file(&hub, id, rid, what, &req)).await,
        // Its record once it closed, as kiln's store of bytes keeps it (`crate::harness::archive`).
        (&Method::GET, ["api", "harness", "sessions", id, "transcripts"]) => Ok(srv::json(StatusCode::OK, &json!({ "pieces": boxed(|| crate::harness::archive::archived(&hub, id)).await? }))),
        (&Method::GET, ["api", "harness", "sessions", id, "transcripts", source]) => boxed(|| archived_transcript(&hub, id, source)).await,
        (&Method::GET, ["api", "harness", "sessions", id, "tasks", task, "output"]) => {
            let since = q.get("since").and_then(|s| s.parse::<u64>().ok());
            match boxed(|| hub.harness.task_output(&hub, id, task, since)).await {
                Ok(Some(v)) => Ok(srv::json(StatusCode::OK, &v)),
                Ok(None) => Err(err(StatusCode::NOT_FOUND, "no such agent session")),
                Err(e) => Err(err(StatusCode::BAD_GATEWAY, format!("{e:#}"))),
            }
        }
        _ => Err(err(StatusCode::NOT_FOUND, "not found")),
    }
}

// ------------------------------------------------------------------ files in conversations

/// The part of a `size`-byte file a request asks for (`Range: bytes=…`): `Ok(None)` for all of it,
/// `Err` when it asks for none of it.
fn byte_range(req: &Request, size: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(v) = srv::header_str(req, &header::RANGE) else { return Ok(None) };
    let Some(spec) = v.trim().strip_prefix("bytes=") else { return Ok(None) };
    // More than one part: all of it (a player asks for one).
    if spec.contains(',') {
        return Ok(None);
    }
    let (a, b) = spec.split_once('-').ok_or(())?;
    let last = size.checked_sub(1).ok_or(())?;
    let (start, end) = match (a.trim(), b.trim()) {
        ("", n) => {
            let n: u64 = n.parse().map_err(|_| ())?;
            if n == 0 {
                return Err(());
            }
            (size.saturating_sub(n), last)
        }
        (s, "") => (s.parse().map_err(|_| ())?, last),
        (s, e) => (s.parse().map_err(|_| ())?, e.parse::<u64>().map_err(|_| ())?.min(last)),
    };
    if start > end {
        return Err(());
    }
    Ok(Some((start, end)))
}

/// `Content-Disposition` for a file shown in place, under its own name.
fn inline_named(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    let mut enc = String::with_capacity(name.len());
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_') {
            enc.push(char::from(b));
        } else {
            enc.push_str(&format!("%{b:02X}"));
        }
    }
    format!("inline; filename*=UTF-8''{enc}")
}

/// A file shown in a conversation: its type (one that can run code, SVG or HTML, runs none: it's
/// sandboxed, away from the session), how long a copy holds, and which part of it this is.
fn file_response(meta: &crate::harness::files::Meta, path: &str, etag: &str, immutable: bool, range: Option<(u64, u64)>, body: srv::Body) -> Response {
    let (status, len) = match range {
        Some((a, b)) => (StatusCode::PARTIAL_CONTENT, b - a + 1),
        None => (StatusCode::OK, meta.size),
    };
    let mime = if meta.mime.is_empty() { "application/octet-stream" } else { meta.mime.as_str() };
    let mut r = srv::response(status, mime, body);
    let h = r.headers_mut();
    h.insert(header::CONTENT_LENGTH, header::HeaderValue::from(len));
    h.insert(header::ACCEPT_RANGES, header::HeaderValue::from_static("bytes"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, header::HeaderValue::from_static("nosniff"));
    h.insert(header::CACHE_CONTROL, header::HeaderValue::from_static(if immutable { "private, max-age=31536000, immutable" } else { "private, no-cache" }));
    if let Ok(v) = header::HeaderValue::from_str(etag) {
        h.insert(header::ETAG, v);
    }
    if let Ok(v) = header::HeaderValue::from_str(&inline_named(path)) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    if let Some((a, b)) = range
        && let Ok(v) = header::HeaderValue::from_str(&format!("bytes {a}-{b}/{}", meta.size))
    {
        h.insert(header::CONTENT_RANGE, v);
    }
    if mime.starts_with("image/svg") || mime.starts_with("text/html") || mime.starts_with("text/xml") {
        h.insert(header::CONTENT_SECURITY_POLICY, header::HeaderValue::from_static("default-src 'none'; img-src data:; style-src 'unsafe-inline'; sandbox"));
    }
    r
}

/// What a page agents show may do in the viewer's browser: run its scripts and forms in a sandbox
/// of its own (an opaque origin: no kiln cookies or storage, nothing of the page it's framed in),
/// and load only its own files (its copies, kiln's fonts, what it makes itself). Who may frame it
/// follows (`frame-ancestors`): kiln.
const PAGE_POLICY: &str = "sandbox allow-scripts allow-forms allow-popups allow-popups-to-escape-sandbox allow-downloads allow-pointer-lock; \
default-src 'self' data: blob:; script-src 'self' 'unsafe-inline' 'unsafe-eval' data: blob:; style-src 'self' 'unsafe-inline' data:; \
img-src 'self' data: blob:; media-src 'self' data: blob:; font-src 'self' data:; connect-src 'self' data: blob:; \
worker-src 'self' data: blob:; frame-src 'self' data: blob:; form-action 'none'; base-uri 'none'";

/// The origin of a URL (`https://host[:port]`), when it has one.
fn origin_of(url: &str) -> Option<String> {
    let uri: http::Uri = url.trim().parse().ok()?;
    let scheme = uri.scheme_str().filter(|s| matches!(*s, "http" | "https"))?;
    Some(format!("{scheme}://{}", uri.authority()?.as_str().to_ascii_lowercase()))
}

/// Pages' own site (`[server] pages_url`): its host, and its port when it says one.
fn pages_site(hub: &Hub) -> Option<(String, Option<u16>)> {
    let uri: http::Uri = hub.cfg.server.pages_url.as_deref()?.trim().parse().ok()?;
    Some((uri.host()?.to_ascii_lowercase(), uri.port_u16()))
}

/// Whether `req` was sent to `site`.
fn asks_for(req: &Request, (host, port): &(String, Option<u16>)) -> bool {
    let asked = req.uri().authority().map(|a| a.as_str().to_owned()).or_else(|| srv::header_str(req, &header::HOST).map(str::to_owned));
    let Some(asked) = asked.and_then(|a| format!("http://{a}/").parse::<http::Uri>().ok()) else { return false };
    asked.host().is_some_and(|h| h.eq_ignore_ascii_case(host)) && (port.is_none() || asked.port_u16() == *port)
}

/// `GET /v/...` on kiln's own address when pages have one of their own: there.
fn to_pages(hub: &Hub, req: &Request) -> Response {
    let base = hub.cfg.server.pages_url.as_deref().unwrap_or("").trim_end_matches('/');
    let to = format!("{base}{}", req.uri().path_and_query().map_or("/", http::uri::PathAndQuery::as_str));
    let mut r = srv::status(StatusCode::TEMPORARY_REDIRECT);
    if let Ok(v) = header::HeaderValue::from_str(&to) {
        r.headers_mut().insert(header::LOCATION, v);
    }
    r.headers_mut().insert(header::REFERRER_POLICY, header::HeaderValue::from_static("no-referrer"));
    r
}

/// Who may frame a page: kiln. On kiln's own address, itself; on pages' own, kiln's addresses
/// (its public one, its runners' front doors).
fn page_ancestors(hub: &Hub) -> String {
    if hub.cfg.server.pages_url.is_none() {
        return "frame-ancestors 'self'".to_owned();
    }
    let mut origins: Vec<String> = std::iter::once(hub.cfg.server.public_url.clone()).chain(hub.front_doors.origins()).filter_map(|u| origin_of(&u)).collect();
    origins.sort();
    origins.dedup();
    format!("frame-ancestors {}", if origins.is_empty() { "'none'".to_owned() } else { origins.join(" ") })
}

/// Page `id`'s own files and kiln's fonts by the address `req` reached them at, for its policy to
/// name beside `'self'`: in a document its frame sandboxes (the conversation's frame does), WebKit,
/// every browser on iOS, matches `'self'` with nothing, and phones were refused the page's own
/// scripts, pictures and fonts. Only a name and a port make one (no header adds to the policy).
fn page_sources(hub: &Hub, req: &Request, id: &str) -> String {
    let asked = req.uri().authority().map(|a| a.as_str().to_owned()).or_else(|| srv::header_str(req, &header::HOST).map(str::to_owned)).unwrap_or_default();
    named_sources(&asked, is_secure(hub, req), id)
}

/// [`page_sources`] for a page reached at `asked` (its `Host`), over HTTPS when `secure`.
fn named_sources(asked: &str, secure: bool, id: &str) -> String {
    let asked = asked.to_ascii_lowercase();
    if asked.is_empty() || !asked.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':')) {
        return String::new();
    }
    let scheme = if secure { "https" } else { "http" };
    format!(" {scheme}://{asked}/v/{id}/ {scheme}://{asked}/v/_/fonts/")
}

/// [`PAGE_POLICY`] with `sources` beside each `'self'`, then who may frame it.
fn page_policy(sources: &str, ancestors: &str) -> String {
    format!("{}; {ancestors}", PAGE_POLICY.replace("'self'", &format!("'self'{sources}")))
}

/// Headers every answer of page `id`'s carries: its sandbox, no referrer (its address is its key),
/// and to any origin (the page's own is opaque), for fonts and modules.
fn page_headers(hub: &Hub, req: &Request, id: &str, h: &mut header::HeaderMap) {
    let policy = page_policy(&page_sources(hub, req, id), &page_ancestors(hub));
    if let Ok(v) = header::HeaderValue::from_str(&policy) {
        h.insert(header::CONTENT_SECURITY_POLICY, v);
    }
    h.insert(header::REFERRER_POLICY, header::HeaderValue::from_static("no-referrer"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, header::HeaderValue::from_static("nosniff"));
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, header::HeaderValue::from_static("*"));
    h.insert(header::HeaderName::from_static("x-robots-tag"), header::HeaderValue::from_static("noindex, nofollow"));
}

/// `GET /v/:id/...`: a page an agent showed, and the files it uses, each at the path it had on its
/// machine (`crate::harness::visuals`); `/v/:id/` itself leads to the page. In ranges, for media.
async fn page(hub: &Arc<Hub>, id: &str, req: &Request) -> R {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let missing = || err(StatusCode::NOT_FOUND, "no such page or file (its conversation may have closed)");
    let h = hub.clone();
    let id_owned = id.to_owned();
    let v = crate::rt::blocking(move || h.harness.visuals.get(&id_owned)).await.ok().flatten().ok_or_else(missing)?;
    let raw = req.uri().path();
    let rest = raw.strip_prefix(&format!("/v/{id}")).unwrap_or("");
    if rest.is_empty() || rest == "/" {
        let mut r = srv::status(StatusCode::FOUND);
        if let Ok(to) = header::HeaderValue::from_str(&format!("/v/{}{}", v.id, crate::harness::visuals::url_path(&v.entry))) {
            r.headers_mut().insert(header::LOCATION, to);
        }
        page_headers(hub, req, &v.id, r.headers_mut());
        return Ok(r);
    }
    let at = srv::decode(rest);
    let used = v.files.iter().find(|f| f.path == at).cloned().ok_or_else(missing)?;
    let blob = hub.harness.visuals.blob(&used.sha);
    let b = blob.clone();
    let size = crate::rt::blocking(move || std::fs::metadata(b).map(|m| m.len())).await.ok().and_then(Result::ok).ok_or_else(missing)?;
    let meta = crate::harness::files::Meta { size, mtime: 0, mime: used.mime.clone(), width: None, height: None };
    let etag = format!("\"{}\"", used.sha);
    if let Some(mut r) = unchanged(req, &etag, true) {
        page_headers(hub, req, &v.id, r.headers_mut());
        return Ok(r);
    }
    let Ok(range) = byte_range(req, size) else { return Ok(unsatisfiable(size)) };
    let (start, end) = range.unwrap_or((0, size.saturating_sub(1)));
    let (tx, rx) = read_ahead(READ_AHEAD);
    crate::rt::spawn(async move {
        let Ok(Ok(mut file)) = crate::rt::blocking(move || std::fs::File::open(&blob)).await else { return };
        let mut at = start;
        while size > 0 && at <= end {
            let want = (end - at + 1).min(256 * 1024);
            let read = crate::rt::blocking(move || -> std::io::Result<(std::fs::File, Vec<u8>)> {
                file.seek(SeekFrom::Start(at))?;
                let mut buf = Vec::new();
                (&mut file).take(want).read_to_end(&mut buf)?;
                Ok((file, buf))
            })
            .await;
            let Ok(Ok((f, buf))) = read else { return };
            if buf.is_empty() {
                return;
            }
            file = f;
            at += buf.len() as u64;
            if tx.send(Bytes::from(buf)).await.is_err() {
                return;
            }
        }
    });
    let name = used.path.rsplit('/').next().unwrap_or("file").to_owned();
    let mut r = file_response(&meta, &name, &etag, true, range, srv::channel_body(rx));
    // The page's own policy, for it and for every file of it (a script or a style is the page's).
    page_headers(hub, req, &v.id, r.headers_mut());
    Ok(r)
}

/// `GET /v/_/fonts/:name`: the fonts kiln's console reads in, for pages to read in too.
fn page_font(name: &str) -> Response {
    let Some(data) = crate::ui::font(name) else { return srv::status(StatusCode::NOT_FOUND) };
    let mut r = srv::response(StatusCode::OK, "font/woff2", srv::full(data));
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, header::HeaderValue::from_static("public, max-age=86400"));
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, header::HeaderValue::from_static("*"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, header::HeaderValue::from_static("nosniff"));
    r
}

/// The answer when a viewer already has this version (`If-None-Match`).
fn unchanged(req: &Request, etag: &str, immutable: bool) -> Option<Response> {
    let have = srv::header_str(req, &header::IF_NONE_MATCH)?;
    if !have.split(',').any(|t| t.trim() == etag || t.trim() == "*") {
        return None;
    }
    let mut r = srv::status(StatusCode::NOT_MODIFIED);
    if let Ok(v) = header::HeaderValue::from_str(etag) {
        r.headers_mut().insert(header::ETAG, v);
    }
    r.headers_mut().insert(header::CACHE_CONTROL, header::HeaderValue::from_static(if immutable { "private, max-age=31536000, immutable" } else { "private, no-cache" }));
    Some(r)
}

/// A range past the end of a `size`-byte file.
fn unsatisfiable(size: u64) -> Response {
    let mut r = srv::status(StatusCode::RANGE_NOT_SATISFIABLE);
    if let Ok(v) = header::HeaderValue::from_str(&format!("bytes */{size}")) {
        r.headers_mut().insert(header::CONTENT_RANGE, v);
    }
    r
}

/// `GET /api/harness/sessions/:id/files/:sha`: a file one of the conversation's messages
/// mentioned, as the server kept it (`crate::harness::kept`). That never changes: cached for good.
async fn harness_kept_file(hub: &Arc<Hub>, id: &str, sha: &str, req: &Request) -> R {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let gone = || err(StatusCode::NOT_FOUND, "no such file in this conversation");
    let kept = hub.harness.keeper.find(id, sha).ok_or_else(gone)?;
    let blob = hub.harness.keeper.blob(sha).ok_or_else(gone)?;
    let meta = kept.meta.clone().unwrap_or_default();
    let etag = format!("\"{}\"", sha.to_ascii_lowercase());
    if let Some(r) = unchanged(req, &etag, true) {
        return Ok(r);
    }
    let Ok(range) = byte_range(req, meta.size) else { return Ok(unsatisfiable(meta.size)) };
    let (start, end) = range.unwrap_or((0, meta.size.saturating_sub(1)));
    let (tx, rx) = read_ahead(READ_AHEAD);
    let size = meta.size;
    crate::rt::spawn(async move {
        let Ok(Ok(mut file)) = crate::rt::blocking(move || std::fs::File::open(&blob)).await else { return };
        let mut at = start;
        while size > 0 && at <= end {
            let want = (end - at + 1).min(256 * 1024);
            let read = crate::rt::blocking(move || -> std::io::Result<(std::fs::File, Vec<u8>)> {
                file.seek(SeekFrom::Start(at))?;
                let mut buf = Vec::new();
                (&mut file).take(want).read_to_end(&mut buf)?;
                Ok((file, buf))
            })
            .await;
            let Ok(Ok((f, buf))) = read else { return };
            if buf.is_empty() {
                return;
            }
            file = f;
            at += buf.len() as u64;
            if tx.send(Bytes::from(buf)).await.is_err() {
                return;
            }
        }
    });
    Ok(file_response(&meta, &kept.path, &etag, true, range, srv::channel_body(rx)))
}

/// `GET /api/harness/sessions/:id/file?path=`: a file one of the conversation's messages mentions,
/// read on its machine now: one that isn't kept (too big, past a budget, from before kiln kept
/// files), in pieces as the viewer takes it.
async fn harness_file(hub: &Arc<Hub>, id: &str, path: Option<String>, req: &Request) -> R {
    let path = path.ok_or_else(|| err(StatusCode::BAD_REQUEST, "which file? (?path=…)"))?;
    let location = hub.harness.mentioned_where(id, &path).ok_or_else(|| err(StatusCode::NOT_FOUND, "no message of this conversation mentions that file"))?;
    serve_from(hub, id, location, path, req).await
}

/// `GET /api/harness/sessions/:id/recordings/:recording/:what` (`video`, `inputs`, `test`): a
/// recording of one of its streams, from its machine (`crate::browser::record`); its log and its
/// test as text to keep.
async fn recording_file(hub: &Arc<Hub>, id: &str, rid: &str, what: &str, req: &Request) -> R {
    let rec = hub.harness.streams.recordings(id, person(req).as_deref()).into_iter().find(|r| r.id == rid).ok_or_else(|| err(StatusCode::NOT_FOUND, "no such recording"))?;
    let path = rec.file(what).ok_or_else(|| err(StatusCode::NOT_FOUND, "a recording's files are its video, inputs and test"))?.to_owned();
    let mut r = match rec.kept.get(what) {
        // Copied into kiln's store of bytes: served from there, its machine there or not.
        Some(meta) => {
            let etag = format!("W/\"{}-{}\"", meta.size, meta.mtime);
            if let Some(r) = unchanged(req, &etag, false) {
                return Ok(r);
            }
            let Ok(range) = byte_range(req, meta.size) else { return Ok(unsatisfiable(meta.size)) };
            let (start, end) = range.unwrap_or((0, meta.size.saturating_sub(1)));
            let key = crate::harness::streams::recording_key(id, rid, what);
            let body = if meta.size == 0 { srv::channel_body(futures::stream::empty()) } else { srv::channel_body(crate::blobs::io::read_stream(Arc::clone(&hub.blobs.blobs), key, None, start, end)) };
            file_response(meta, &path, &etag, false, range, body)
        }
        None => serve_from(hub, id, crate::harness::registry::Location::Machine(rec.machine.clone()), path.clone(), req).await?,
    };
    if what != "video" {
        let name = std::path::Path::new(&path).file_name().map(|n| n.to_string_lossy().replace('"', "")).unwrap_or_default();
        r.headers_mut().insert(header::CONTENT_TYPE, header::HeaderValue::from_static("text/plain; charset=utf-8"));
        if let Ok(v) = header::HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
            r.headers_mut().insert(header::CONTENT_DISPOSITION, v);
        }
    }
    Ok(r)
}

/// `GET /api/harness/sessions/:id/transcripts/:source`: a closed conversation's record from
/// `source`, every line archived, in order (JSON lines, read back from its pieces).
async fn archived_transcript(hub: &Arc<Hub>, id: &str, source: &str) -> R {
    let pieces: Vec<_> = crate::harness::archive::archived(hub, id).await?.into_iter().filter(|p| p.source == source).collect();
    if pieces.is_empty() {
        return Err(err(StatusCode::NOT_FOUND, "no record of that source archived for this conversation"));
    }
    // Its length, so a reader told of every piece knows when one was missing.
    let length = pieces.iter().fold(0u64, |n, p| n.saturating_add(p.raw));
    let r = srv::response(StatusCode::OK, "application/x-ndjson", srv::channel_body(crate::harness::archive::lines(hub, pieces)));
    Ok(srv::with_header(r, header::CONTENT_LENGTH, &length.to_string()))
}

/// A file of session `id`'s machine (`location`), in ranges, as it reads it.
async fn serve_from(hub: &Arc<Hub>, id: &str, location: crate::harness::registry::Location, path: String, req: &Request) -> R {
    use crate::harness::kept::piece;
    // What it is, and how big, before the part asked for.
    let (answer, _) = piece(hub, &location, id, &path, 0, 0)
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("{e:#}")))?
        .ok_or_else(|| err(StatusCode::NOT_IMPLEMENTED, "this agent's machine runs a kiln too old to show files: upgrade its runner"))?;
    let Some(meta) = answer.meta else { return Err(err(StatusCode::NOT_FOUND, answer.error.unwrap_or_else(|| "it can't be read".to_owned()))) };
    let etag = format!("W/\"{}-{}\"", meta.size, meta.mtime);
    if let Some(r) = unchanged(req, &etag, false) {
        return Ok(r);
    }
    let Ok(range) = byte_range(req, meta.size) else { return Ok(unsatisfiable(meta.size)) };
    let (start, end) = range.unwrap_or((0, meta.size.saturating_sub(1)));
    let (tx, rx) = read_ahead(MACHINE_AHEAD);
    let (h, session, p, m) = (hub.clone(), id.to_owned(), path.clone(), meta.clone());
    crate::rt::spawn(async move {
        let mut at = start;
        while m.size > 0 && at <= end {
            let want = u32::try_from((end - at + 1).min(u64::from(crate::harness::files::PIECE))).unwrap_or(crate::harness::files::PIECE);
            let Ok(Some((a, d))) = piece(&h, &location, &session, &p, at, want).await else { return };
            // It changed on the way: stop, and the viewer asks again.
            if a.meta.as_ref().is_none_or(|n| n.size != m.size || n.mtime != m.mtime) || d.is_empty() {
                return;
            }
            at += d.len() as u64;
            if tx.send(Bytes::from(d)).await.is_err() {
                return;
            }
        }
    });
    Ok(file_response(&meta, &path, &etag, false, range, srv::channel_body(rx)))
}

/// A front-door handler's answer as the API's.
fn front_door(a: crate::frontdoor::control::Answer) -> R {
    a.map_err(|(code, msg)| err(code, msg))
}

// ------------------------------------------------------------------ AI sessions

/// The AI coding sessions running on every connected machine, most recently active first.
fn ai_sessions(hub: &Hub, caller: &crate::authz::Caller) -> Response {
    // Conversations kiln already runs as agent sessions (taken over, or resumed by hand there too).
    let managed: HashMap<String, String> = hub.harness.list().into_iter().filter_map(|i| Some((i.harness_session?, i.id))).collect();
    let mut out: Vec<serde_json::Value> = Vec::new();
    for (machine, sessions) in hub.ai_sessions.load().iter() {
        // Only the machines whose terminal the caller may use (docs/agents.md, "Whose agents").
        if !caller.can("runners.terminal", &crate::harness::lineage::machine_scope(&format!("agent:{machine}"))) {
            continue;
        }
        for s in sessions.iter() {
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
fn ai_silent(hub: &Hub, caller: &crate::authz::Caller) -> Response {
    let reporting: std::collections::HashSet<String> = hub.ai_sessions.load().keys().cloned().collect();
    let mut machines: Vec<String> = hub
        .agents
        .load()
        .values()
        .filter(|a| a.connected && !a.ephemeral && !reporting.contains(&a.name))
        .filter(|a| caller.can("runners.terminal", &crate::harness::lineage::machine_scope(&format!("agent:{}", a.name))))
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
async fn ai_takeover(hub: &Hub, caller: &crate::authz::Caller, machine: &str, pid: &str, q: &HashMap<String, String>, req: Request) -> R {
    use crate::aisessions::TakeOver;
    let pid: u32 = pid.parse().map_err(|_| err(StatusCode::BAD_REQUEST, "bad pid"))?;
    let body = srv::body_bytes(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let opts: TakeoverOpts = if body.iter().all(u8::is_ascii_whitespace) {
        TakeoverOpts::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?
    };
    let stop = opts.stop || q.get("stop").is_some_and(|v| v == "1" || v == "true");
    let found = hub.ai_sessions.load().get(machine).and_then(|l| l.iter().find(|s| s.pid == pid).cloned());
    let Some(found) = found else {
        return Err(err(StatusCode::NOT_FOUND, "no such session on that machine (it may have ended: resume it with spec.resume)"));
    };
    // Before anything of it is ended: refused here, it goes on running where it is.
    let target = found_target(hub, caller, machine, "take over a session found on that machine")?;
    let held = found.terminal.as_deref().is_some_and(|t| hub.terminals.get(t).is_some());
    let spec = match (takeover_on(hub, machine, found.takeover(held)), found.takeover_spec(held)) {
        (TakeOver::Ready, Some(spec)) => spec,
        (TakeOver::Stop, Some(spec)) if stop => spec,
        (TakeOver::Stop, _) => {
            return Err(err(StatusCode::CONFLICT, format!("it runs outside kiln (process {pid}): taking it over ends it there, so ask with {{\"stop\": true}}")));
        }
        (TakeOver::Upgrade, _) => {
            return Err(err(StatusCode::CONFLICT, format!("{}'s runner is older than this take-over: upgrade it in System → Updates first", hub.shown(machine))));
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
    let infos = hub.harness.open(hub, &target, spec, 1).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    opened_by(hub, caller, &infos).await;
    Ok(srv::json(StatusCode::OK, &infos))
}

/// Who opened `infos`, kept: the agents act for them (`crate::mcp::acts_for`, when kiln may act for
/// them with the token they used), and are theirs (`crate::mcp::whose`, docs/agents.md "Whose
/// agents").
async fn opened_by(hub: &Hub, caller: &crate::authz::Caller, infos: &[crate::harness::session::Info]) {
    let (opener, person) = (caller.opener(), crate::harness::lineage::person_of(caller));
    for i in infos {
        if let Some(o) = &opener {
            let _ = hub.store.kv_set(crate::mcp::opener_key(&i.id), o.clone()).await;
        }
        if let Some(p) = &person {
            let _ = hub.store.kv_set(crate::mcp::opened_by_key(&i.id), p.clone()).await;
        }
    }
}

/// Where a session found on `machine` continues as an agent: on that machine's runner, except the
/// server's own, which runs no agents (`crate::harness::limit`): they're the server's (`local`), on
/// the same machine, under the server's right to run them.
fn found_target(hub: &Hub, caller: &crate::authz::Caller, machine: &str, what: &str) -> Result<String, ApiError> {
    if !crate::harness::limit::own(hub, machine) {
        return Ok(format!("agent:{machine}"));
    }
    may_run_on(caller, "local", what)?;
    if !hub.cfg.terminal.enabled {
        return Err(err(StatusCode::FORBIDDEN, "terminals (and so agents) are disabled on this server"));
    }
    Ok("local".to_owned())
}

/// [`crate::harness::lineage::may_run_on`], as the API answers it.
fn may_run_on(caller: &crate::authz::Caller, target: &str, what: &str) -> Result<(), ApiError> {
    crate::harness::lineage::may_run_on(caller, target, what).map_err(|(c, m)| err(c, m))
}

/// Continue a copy of a session found running on a machine as a new kiln agent session (a branch
/// of its conversation, in the same folder), leaving the original running as it is.
async fn ai_fork(hub: &Hub, caller: &crate::authz::Caller, machine: &str, pid: &str) -> R {
    let pid: u32 = pid.parse().map_err(|_| err(StatusCode::BAD_REQUEST, "bad pid"))?;
    let found = hub.ai_sessions.load().get(machine).and_then(|l| l.iter().find(|s| s.pid == pid).cloned());
    let Some(found) = found else {
        return Err(err(StatusCode::NOT_FOUND, "no such session on that machine (it may have ended: resume it with spec.resume)"));
    };
    if !forks_on(hub, machine) {
        return Err(err(StatusCode::CONFLICT, format!("{}'s runner is older than branching found sessions: upgrade it in System → Updates first", hub.shown(machine))));
    }
    let Some(spec) = found.fork_spec() else {
        return Err(if !found.started() {
            err(StatusCode::CONFLICT, "it has no conversation to continue yet")
        } else {
            err(StatusCode::BAD_REQUEST, format!("kiln cannot branch a {} session", found.tool))
        });
    };
    let target = found_target(hub, caller, machine, "branch a session found on that machine")?;
    hub.store.audit("admin".into(), "agent.fork".into(), format!("{} (process {pid}) on {machine}", found.tool)).await;
    let infos = hub.harness.open(hub, &target, spec, 1).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    opened_by(hub, caller, &infos).await;
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
    let machine = sync_machine(hub, req);
    (!machine.is_empty() && bearer(req.headers()).is_some_and(|t| crate::util::ct_eq(t.as_bytes(), hub.sync.token(&machine).as_bytes())))
        || authed(hub, req.headers())
}

/// What happened in the synced repos (`?since=` the number to start from, `&repo=`, `&limit=`):
/// the newest events first, and the number the next one gets.
fn sync_activity(hub: &Hub, req: &Request) -> Response {
    let q = srv::query(req);
    let since = q.get("since").and_then(|v| v.parse().ok()).unwrap_or(0);
    let limit = q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(100).min(500);
    let repo = q.get("repo").map(String::as_str).filter(|r| !r.is_empty());
    let (events, next) = hub.sync.activity(since, repo, limit);
    srv::json(StatusCode::OK, &serde_json::json!({ "events": events, "next": next }))
}

/// A machine's request for contents, counted: `direction` `up` (sent, or asked whether the server
/// has it) or `down` (fetched). Its machine is named only once its token checked out.
fn sync_blob_counted(machine: &str, direction: &str, result: &str, bytes: u64) {
    let m = crate::metrics::sync();
    m.blob_requests.inc(&[machine, direction, result]);
    if bytes > 0 {
        m.blob_bytes.add(&[machine, direction], bytes);
    }
}

/// The machine a blob request names: by its id, whatever name the runner's certificate carries
/// (docs/machines.md, "One record a machine").
fn sync_machine(hub: &Hub, req: &Request) -> String {
    srv::query(req).get("machine").filter(|m| !m.is_empty()).map(|m| hub.machine_of(m)).unwrap_or_default()
}

/// Copies of kiln.db for learners are made one at a time (each reads the whole database), in the order asked for:
/// a gate with no lock (`crate::turn`; goal 5, plan 01a1a59c2aae, its top goal), where async-lock's semaphore waited
/// on event-listener's `Mutex`.
static CLUSTER_COPIES: crate::turn::Gate = crate::turn::Gate::one();

/// A copy of kiln.db for a runner's learner, made now and streamed: for the runner's token for its
/// machine (`?machine=`), or the admin token. The copy goes once sent. kiln hands it on by its
/// SHA-256 to the runner process that asks for it (`?instance=`, a learner that signs its receipt),
/// saying so in a header (`crate::custody::cluster::COPY_HEADER`; docs/ci.md W27-R08).
async fn cluster_copy(hub: &Hub, req: &Request) -> R {
    let q = srv::query(req);
    let machine = q.get("machine").map(|m| hub.machine_of(m)).unwrap_or_default();
    let instance = q.get("instance").filter(|i| !i.is_empty()).cloned();
    let theirs = !machine.is_empty() && bearer(req.headers()).is_some_and(|t| crate::util::ct_eq(t.as_bytes(), crate::store::learner::token(&hub.admin_token, &machine).as_bytes()));
    if !theirs && !authed(hub, req.headers()) {
        return Err(err(StatusCode::UNAUTHORIZED, "bad cluster token"));
    }
    let dir = hub.cfg.data("cluster").join("copies");
    let _one = CLUSTER_COPIES.turn().await;
    // Copies a stop left behind.
    if let Ok(old) = std::fs::read_dir(&dir) {
        for e in old.flatten() {
            std::fs::remove_file(e.path()).ok();
        }
    }
    std::fs::create_dir_all(&dir).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("making {}: {e}", dir.display())))?;
    let path = dir.join(format!("{}.db", crate::util::new_id()));
    let started = Instant::now();
    if let Err(e) = hub.store.copy_for_learner(path.clone()).await {
        std::fs::remove_file(&path).ok();
        return Err(err(StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")));
    }
    let handed = if theirs { crate::custody::cluster::copy_hand_on(hub, &machine, instance.as_deref(), &path).await } else { None };
    if handed.is_some() {
        copy_changed(&path).await;
    }
    let (size, rx) = copy_body(path).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("reading the copy of kiln.db: {e}")))?;
    tracing::info!(machine, bytes = size, took_ms = started.elapsed().as_millis() as u64, handed_on = handed.is_some(), "a copy of kiln.db for a runner's learner");
    let r = srv::response(StatusCode::OK, "application/octet-stream", srv::channel_body(rx));
    let r = srv::with_header(r, header::CONTENT_LENGTH, &size.to_string());
    Ok(match handed {
        Some(h) => srv::with_header(r, header::HeaderName::from_static(crate::custody::cluster::COPY_HEADER), &h),
        None => r,
    })
}

/// A test build's `copy_changed` (the server's, docs/ci.md W27-R08): the first copy of kiln.db kiln
/// hands on to a learner has a byte in its middle changed after kiln hashed it, once, as one changed
/// on its way. The learner refuses it and fetches another.
async fn copy_changed(path: &std::path::Path) {
    static CHANGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !crate::runner::chaos::get().is_some_and(|c| c.copy_changed) || CHANGED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let path = path.to_path_buf();
    let changed = crate::rt::blocking(move || -> std::io::Result<()> {
        use std::io::{Read as _, Seek as _, Write as _};
        let mut f = std::fs::OpenOptions::new().read(true).write(true).open(&path)?;
        let middle = f.metadata()?.len() / 2;
        let mut b = [0u8; 1];
        f.seek(std::io::SeekFrom::Start(middle))?;
        f.read_exact(&mut b)?;
        let [byte] = b;
        f.seek(std::io::SeekFrom::Start(middle))?;
        f.write_all(&[byte ^ 0x01])
    })
    .await
    .and_then(|r| r.map_err(anyhow::Error::from));
    match changed {
        Ok(()) => tracing::warn!(target: "kiln.custody", "chaos: a copy of kiln.db handed on to a learner changed on its way"),
        Err(e) => tracing::warn!(target: "kiln.custody", error = %format!("{e:#}"), "chaos: a copy of kiln.db couldn't be changed"),
    }
}

/// A file's pieces of 256 KiB read ahead of the viewer that asked for it, at the most: as many as the channel
/// they waited in held (a MiB a viewer). Its reader waits for room past them.
const READ_AHEAD: usize = 4;

/// A session's machine's file's pieces ([`crate::harness::files::PIECE`]) read ahead of its viewer, at the
/// most: as many as the channel they waited in held.
const MACHINE_AHEAD: usize = 2;

/// A learner's copy of kiln.db read ahead of its runner, at the most: sixteen pieces of 256 KiB (four MiB), as
/// many as the channel they waited in held.
const COPY_AHEAD: usize = 16;

/// A response's pieces read ahead of the connection that sends them (goal 4, plan 01a1a59c2a9c step 5):
/// `ahead` of them whatever each carries, lock-free (`crate::memory::queue`: an async-channel's wait took
/// event-listener's `Mutex`, the reader sending on its shard or thread and the connection waiting on its own),
/// the reader waiting for room past them.
fn read_ahead(ahead: usize) -> (crate::memory::queue::Sender<Bytes>, crate::memory::queue::Receiver<Bytes>) {
    crate::memory::queue::places(&crate::memory::owners::CHANNELS, ahead)
}

/// Streams the copy at `path` (it goes once sent): its size, and its bytes as they're read, from
/// the file opened now. The next copy is made once this one is handed over and clears what a stop
/// left behind, this copy's name among it: an open file outlives its name (and Windows keeps it).
fn copy_body(path: std::path::PathBuf) -> std::io::Result<(u64, crate::memory::queue::Receiver<Bytes>)> {
    let f = std::fs::File::open(&path)?;
    let size = f.metadata()?.len();
    let (tx, rx) = read_ahead(COPY_AHEAD);
    // Read, handed over and removed on the blocking pool, the task only waiting for it (goal 9, plan 01a1a87b2cec step
    // 3): the copy's removal gives the file system back the whole of kiln.db's copy, and done in the task's last poll,
    // on its shard, it held that shard 1.9 to 4.0 ms on the CPU in g9p16's calm rounds (`api.rs:2248`, train 16).
    crate::rt::spawn(async move {
        let _ = crate::rt::blocking(move || copy_out(f, tx, &path)).await;
    });
    Ok((size, rx))
}

/// [`copy_body`]'s job, on the blocking pool: `f` read to `tx` until it ends or no one takes what's read, `tx` let go
/// of (the body ends with that), the file closed, then its name removed (Windows removes an open file's name only once
/// it's closed). Removed however the reading ended.
fn copy_out(mut f: std::fs::File, tx: crate::memory::queue::Sender<Bytes>, path: &std::path::Path) -> std::io::Result<()> {
    let read = read_out(&mut f, &tx);
    drop(tx);
    drop(f);
    std::fs::remove_file(path).ok();
    read
}

/// `f` read to `tx` in pieces of 256 KiB, as many waiting in `tx` as it holds, until `f` ends or no one takes them.
fn read_out(f: &mut std::fs::File, tx: &crate::memory::queue::Sender<Bytes>) -> std::io::Result<()> {
    use std::io::Read as _;
    let mut buf = vec![0u8; 256 << 10];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 || tx.send_blocking(Bytes::copy_from_slice(buf.get(..n).unwrap_or_default())).is_err() {
            return Ok(());
        }
    }
}

/// One of the contents synced repos name (`HEAD`: whether the server has it).
fn sync_blob_get(hub: &Hub, hash: &str, req: &Request) -> R {
    let direction = if req.method() == Method::HEAD { "up" } else { "down" };
    if !sync_token_ok(hub, req) {
        sync_blob_counted("", direction, "unauthorized", 0);
        return Err(err(StatusCode::UNAUTHORIZED, "bad sync token"));
    }
    let machine = sync_machine(hub, req);
    let contents = hub.sync.contents();
    if req.method() == Method::HEAD {
        let held = contents.has(hash);
        sync_blob_counted(&machine, direction, if held { "present" } else { "missing" }, 0);
        return if held { Ok(srv::status(StatusCode::OK)) } else { Err(err(StatusCode::NOT_FOUND, "no such blob")) };
    }
    let Some((bytes, body)) = contents.stream(hash) else {
        sync_blob_counted(&machine, direction, "missing", 0);
        return Err(err(StatusCode::NOT_FOUND, "no such blob"));
    };
    sync_blob_counted(&machine, direction, "ok", bytes);
    let r = srv::response(StatusCode::OK, "application/octet-stream", srv::channel_body(body));
    Ok(srv::with_header(r, header::CONTENT_LENGTH, &bytes.to_string()))
}

/// What a presigned URL is answered when it gives nothing: one answer for a bad signature, another
/// method's or key's URL, an expired one, a body of another length, and no such object, so the
/// route tells its holder nothing it doesn't already know.
fn not_granted() -> ApiError {
    err(StatusCode::FORBIDDEN, "this URL grants nothing here")
}

/// `/api/blobs/<key>?expires=&sig=` (and `&length=` for a put): one of the local or the Mantle
/// store's presigned URLs (docs/blobs.md, `crate::blobs::local`). The URL is the only credential:
/// its signature covers the method, the exact key, the expiry (at most an hour) and a put's
/// length, and is checked in constant time before anything is read or written. A get streams the
/// one object it names, in a range when one is asked; a put keeps a body of exactly the length
/// declared, or nothing. No listing, no prefixes, and keys with `.` or `..` segments are refused as
/// any bad key is. An S3 store's presigned URLs lead to the S3, not here.
async fn presigned(hub: &Arc<Hub>, encoded: &str, req: Request) -> R {
    use kiln_seams::blobs::{Access, Method as Signed, Refusal};
    let signed = if req.method() == Method::PUT { Signed::Put } else { Signed::Get };
    let query = req.uri().query().unwrap_or_default().to_owned();
    let grant = hub.blobs.local.as_ref().map(|l| l.check_presigned(encoded, &query, signed))
        .or_else(|| hub.blobs.mantle.as_ref().map(|m| m.check_presigned(encoded, &query, signed))).ok_or_else(not_granted)?;
    let (key, access) = grant.map_err(|_| not_granted())?;
    let blobs = Arc::clone(&hub.blobs.blobs);
    let failed = |e: Refusal| match e {
        Refusal::Unavailable(_) => err(StatusCode::SERVICE_UNAVAILABLE, e.to_string()),
        _ => not_granted(),
    };
    if let Access::Put { length } = access {
        let said = req.headers().get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
        if said.is_some_and(|n| n != length) {
            return Err(not_granted());
        }
        let mut writer = crate::blobs::io::Writer::new(blobs.as_ref(), &key);
        let mut body = req.into_body();
        let mut got = 0u64;
        loop {
            let chunk = match srv::next_chunk(&mut body).await {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => {
                    writer.abort().await;
                    return Err(err(StatusCode::BAD_REQUEST, e.to_string()));
                }
            };
            got = got.saturating_add(chunk.len() as u64);
            if got > length {
                writer.abort().await;
                return Err(not_granted());
            }
            if let Err(e) = writer.write(&chunk).await {
                writer.abort().await;
                return Err(failed(e));
            }
        }
        if got != length {
            writer.abort().await;
            return Err(not_granted());
        }
        writer.finish().await.map_err(failed)?;
        return Ok(srv::status(StatusCode::OK));
    }
    let meta = blobs.head(&key).await.map_err(failed)?;
    let Ok(range) = byte_range(&req, meta.size) else { return Ok(unsatisfiable(meta.size)) };
    let (start, end) = range.unwrap_or((0, meta.size.saturating_sub(1)));
    let length = if meta.size == 0 { 0 } else { end.saturating_sub(start).saturating_add(1) };
    let body = if req.method() == Method::HEAD || meta.size == 0 { srv::channel_body(futures::stream::empty()) } else { srv::channel_body(crate::blobs::io::read_stream(blobs, key, Some(meta.etag.clone()), start, end)) };
    let mut r = srv::response(if range.is_some() { StatusCode::PARTIAL_CONTENT } else { StatusCode::OK }, "application/octet-stream", body);
    r = srv::with_header(r, header::CONTENT_LENGTH, &length.to_string());
    r = srv::with_header(r, header::ACCEPT_RANGES, "bytes");
    if range.is_some() {
        r = srv::with_header(r, header::CONTENT_RANGE, &format!("bytes {start}-{end}/{}", meta.size));
    }
    Ok(r)
}

/// The largest contents a machine sends (a copy leaves out files over 256 MiB unless told otherwise).
const SYNC_BLOB_MAX: u64 = 1 << 30;
/// Room the server keeps free on its data volume: contents are refused below it.
const SYNC_DISK_FLOOR: u64 = 2 << 30;

/// Contents a machine's steps name: kept in kiln's store of bytes once they hash to their name
/// (`crate::sync::contents`), arriving first in `<data>/sync/incoming`. Refused past
/// [`SYNC_BLOB_MAX`], or while the server's disk has less than [`SYNC_DISK_FLOOR`] free.
async fn sync_blob_put(hub: &Hub, hash: &str, req: Request) -> R {
    if !sync_token_ok(hub, &req) {
        sync_blob_counted("", "up", "unauthorized", 0);
        return Err(err(StatusCode::UNAUTHORIZED, "bad sync token"));
    }
    let machine = sync_machine(hub, &req);
    let counted = |result: &str, bytes: u64| sync_blob_counted(&machine, "up", result, bytes);
    let contents = hub.sync.contents();
    if !crate::sync::blobs::valid_hash(hash) {
        counted("refused", 0);
        return Err(err(StatusCode::BAD_REQUEST, "not a SHA-256"));
    }
    if contents.has(hash) {
        counted("present", 0);
        return Ok(srv::status(StatusCode::OK));
    }
    let length = req.headers().get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
    if length.is_some_and(|n| n > SYNC_BLOB_MAX) {
        counted("refused", 0);
        return Err(err(StatusCode::PAYLOAD_TOO_LARGE, format!("contents are synced up to {} MiB", SYNC_BLOB_MAX >> 20)));
    }
    let incoming = hub.cfg.data("sync").join("incoming");
    if let Err(e) = std::fs::create_dir_all(&incoming) {
        counted("failed", 0);
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, format!("{}: {e}", incoming.display())));
    }
    if crate::os::disk_available(&incoming).is_some_and(|free| free < SYNC_DISK_FLOOR.saturating_add(length.unwrap_or(0))) {
        counted("refused", 0);
        tracing::warn!(target: "kiln.sync", event = "sync.blob_refused", machine = %machine, reason = "disk_full", "the server's disk is nearly full: sync takes no more contents");
        return Err(err(StatusCode::INSUFFICIENT_STORAGE, "the server's disk is nearly full: sync takes no more contents until there is room"));
    }
    let tmp = incoming.join(crate::util::new_id());
    let (sink, done) = crate::rt::file_sink(tmp.clone(), SYNC_BLOB_MAX).inspect_err(|_| counted("failed", 0))?;
    let mut body = req.into_body();
    let mut bytes = 0u64;
    loop {
        let chunk = match srv::next_chunk(&mut body).await {
            Ok(Some(c)) => c,
            Ok(None) => break,
            Err(e) => {
                drop(sink);
                let _ = done.await;
                let _ = std::fs::remove_file(&tmp);
                counted("failed", 0);
                return Err(err(StatusCode::BAD_REQUEST, e.to_string()));
            }
        };
        bytes = bytes.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
        if sink.send(chunk).await.is_err() {
            break;
        }
    }
    drop(sink);
    if let Err(e) = done.await {
        let _ = std::fs::remove_file(&tmp);
        counted("failed", 0);
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")));
    }
    let kept = contents.put_file(hash, &tmp).await;
    let t = tmp.clone();
    let _ = crate::rt::blocking(move || std::fs::remove_file(&t)).await;
    match kept {
        Ok(()) => {
            counted("ok", bytes);
            Ok(srv::status(StatusCode::CREATED))
        }
        Err(kiln_seams::blobs::Refusal::BadChecksum { .. }) => {
            counted("refused", 0);
            Err(err(StatusCode::BAD_REQUEST, "the contents don't hash to that name"))
        }
        Err(e @ kiln_seams::blobs::Refusal::Full { .. }) => {
            counted("refused", 0);
            Err(err(StatusCode::INSUFFICIENT_STORAGE, e.to_string()))
        }
        Err(e) => {
            counted("failed", 0);
            Err(err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
        }
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
    /// Its files (the default), its git refs (`git`), or both (`files_and_git`).
    #[serde(default)]
    mode: crate::sync::Mode,
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
    let ask = crate::sync::server::Ask::Create { name: a.name, copies: a.copies.into_iter().map(|c| (c.machine, c.path)).collect(), exclude: a.exclude, mode: a.mode, transient: None };
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
    let machine = hub.machine_of(q.get("machine").filter(|m| !m.is_empty())?);
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
    if crate::os::disk_available(&hub.cfg.data("sync")).is_some_and(|free| free < SYNC_DISK_FLOOR.saturating_add(length.unwrap_or(0))) {
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
        crate::plans::Error::Refused(c, m) => err(c, m),
        crate::plans::Error::Other(e) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

async fn plans_list(hub: &Hub, q: &HashMap<String, String>) -> R {
    // An agent's (`session`), and the plan each agent proposed (`proposed`), when asked; in brief, for
    // a list that opens each on asking (an agent's Plan tab).
    let session = q.get("session").map(String::as_str).filter(|s| !s.is_empty());
    let proposed = q.get("proposed").is_some_and(|p| p == "1" || p == "true");
    let brief = q.get("brief").is_some_and(|b| b == "1" || b == "true");
    // Kept in memory while it's what the store holds, each plan encoded once (goal 10, plan
    // 01a1a87b2dae step 2); otherwise from the store, as before.
    if let Some(listed) = hub.harness.views.plans.fresh(&hub.store, crate::plans::listed) {
        return Ok(pre_encoded(listed.1.answer(q.get("q").map(String::as_str), q.get("tag").map(String::as_str), session, proposed, brief)));
    }
    let plans = crate::plans::list(hub, q.get("q").cloned(), q.get("tag").cloned()).await.map_err(plan_err)?;
    let plans = crate::plans::of_source(plans, session, proposed);
    if brief {
        return Ok(srv::json(StatusCode::OK, &plans.iter().map(crate::plans::brief).collect::<Vec<_>>()));
    }
    Ok(srv::json(StatusCode::OK, &plans))
}

/// `GET /api/harness/sessions/{id}/goals`: its goal stack, kept in memory while it's what the store
/// holds, each stack encoded once (goal 10, plan 01a1a87b2dae step 2); otherwise from the store.
async fn harness_goals(hub: &Hub, id: &str) -> R {
    if let Some(stacks) = hub.harness.views.goals.fresh(&hub.store, crate::goals::stacks) {
        return stacks.1.answer(id).map(pre_encoded).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")));
    }
    crate::goals::get(hub, id).await.map(|g| srv::json(StatusCode::OK, &g)).map_err(goals_err)
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
        Ok(Some(view)) => Ok(srv::json(StatusCode::OK, &view)),
        Ok(None) => Err(err(StatusCode::NOT_FOUND, "no view for that session (not Claude Code, or no transcript)")),
        Err(e) => Err(err(StatusCode::BAD_GATEWAY, format!("{e:#}"))),
    }
}

async fn ai_transcript(hub: &Hub, machine: &str, pid: &str) -> R {
    let pid: u32 = pid.parse().map_err(|_| err(StatusCode::BAD_REQUEST, "bad pid"))?;
    match hub.ai_transcript(machine, pid).await {
        Ok(Some(entries)) => Ok(srv::json(StatusCode::OK, &entries)),
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
/// One password check at a time, so the throttle cannot be outrun by parallel guesses: in the order they came, with
/// no lock (`crate::turn`; goal 5, plan 01a1a59c2aae, its top goal), where async-lock's mutex waited on
/// event-listener's `Mutex`.
static PASSWORD_GATE: crate::turn::Gate = crate::turn::Gate::one();

/// Which sign-in methods the login page should offer (public).
async fn auth_methods(hub: &Hub) -> Response {
    let path = crate::password::path(&hub.cfg);
    let password = crate::rt::blocking(move || crate::password::load(&path).is_some()).await.unwrap_or(false);
    // Passkeys kept here, and people's own passwords (who then sign in with their name).
    let (passkey, people) = (crate::authz::passkeys::any(hub).await, crate::authz::passwords::any(hub).await);
    srv::json(StatusCode::OK, &json!({ "password": password, "token": true, "passkey": passkey, "people": people }))
}

#[derive(Deserialize)]
struct PasswordLogin {
    password: String,
    /// A person's name: their own password (W10). Without, the owner's.
    #[serde(default)]
    name: Option<String>,
}

async fn login_password(hub: &Hub, req: Request) -> R {
    let secure = is_secure(hub, &req);
    let l: PasswordLogin =
        srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    if let Some(name) = l.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        let (id, _, epoch) = crate::authz::passwords::sign_in(hub, name, &l.password).await.map_err(|(c, m)| err(c, m))?;
        return Ok(srv::with_header(srv::status(StatusCode::NO_CONTENT), header::SET_COOKIE, &cookie_for(&crate::authz::gate::session_value(hub, &id, epoch), secure)));
    }
    let gate = PASSWORD_GATE.turn().await;
    if let Err(wait) = PASSWORD_THROTTLE.check() {
        let secs = wait.as_secs().max(1);
        let r = srv::json(
            StatusCode::TOO_MANY_REQUESTS,
            &json!({ "error": format!("Too many wrong passwords. Try again in {secs}s.") }),
        );
        return Ok(srv::with_header(r, header::RETRY_AFTER, &secs.to_string()));
    }
    let path = crate::password::path(&hub.cfg);
    // The file read on the blocking pool, Argon2's check on the compute pool (goal 3, plan 01a1a59c2a8e step 4).
    let outcome = match crate::rt::blocking(move || crate::password::load(&path)).await {
        Ok(Some(phc)) => Some(crate::rt::compute(move || crate::password::verify(&l.password, &phc)).await.unwrap_or(false)),
        Ok(None) => None,
        Err(_) => Some(false),
    };
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

/// `POST /api/auth/passkey`: a passkey's answer to its challenge; the session of whoever it's
/// theirs (docs/ci.md W10).
async fn login_passkey(hub: &Hub, req: Request) -> R {
    let secure = is_secure(hub, &req);
    let body: serde_json::Value = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    match crate::authz::passkeys::sign_in(hub, &body).await {
        Ok(crate::authz::passkeys::Who::Owner) => Ok(srv::with_header(srv::status(StatusCode::NO_CONTENT), header::SET_COOKIE, &session_cookie(hub, secure))),
        Ok(crate::authz::passkeys::Who::User { id, epoch, .. }) => {
            Ok(srv::with_header(srv::status(StatusCode::NO_CONTENT), header::SET_COOKIE, &cookie_for(&crate::authz::gate::session_value(hub, &id, epoch), secure)))
        }
        Err((c, m)) => {
            crate::rt::sleep(Duration::from_millis(300)).await;
            Err(err(c, m))
        }
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
    // Argon2 on the compute pool, then the file on the blocking pool (goal 3, plan 01a1a59c2a8e step 4: the
    // blocking pool's places are for calls that block, and a password's hashing is CPU alone).
    let hash = crate::rt::compute(move || crate::password::hash(&p.password)).await??;
    crate::rt::blocking(move || crate::password::store(&path, &hash)).await??;
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
    // The owner's, encoded once for what in it can change (goal 10, plan 01a1a87b2dae step 2): it has
    // no groups to read, and the rest is the same each time.
    if let Some(owner) = caller.as_ref().filter(|c| c.is_owner() && c.user.is_none())
        && let Ok(made) = hub.harness.views.me.answer(me_key(hub), |_| crate::reads::encode(&me_json(hub, Some(owner), Vec::new())))
    {
        return pre_named(made);
    }
    // The groups the person is in (kiln's, and their identity providers'): the owner teams they may
    // name for an app (docs/deploy-api.md §3.1).
    let groups = match caller.as_ref().and_then(|c| c.user.clone()) {
        Some(u) => crate::authz::groups::of_user(hub, &u).await.unwrap_or_default(),
        None => Vec::new(),
    };
    srv::json(StatusCode::OK, &me_json(hub, caller.as_ref(), groups))
}

/// What can change in the owner's `GET /api/me` ([`crate::reads::MeKey`]).
fn me_key(hub: &Hub) -> crate::reads::MeKey {
    let d = &hub.discovery;
    crate::reads::MeKey { origins: hub.front_doors.origins(), advert: d.announcing().then(|| d.advert.lock().clone()), hostname: crate::config::Config::hostname() }
}

/// `GET /api/me` for `caller` (none: not signed in), in `groups`.
fn me_json(hub: &Hub, caller: Option<&crate::authz::Caller>, groups: Vec<String>) -> serde_json::Value {
    // What discovery announces anyway (when it does), so a probe can tell which instance answers.
    let d = &hub.discovery;
    let announced = d.announcing().then(|| {
        let a = d.advert.lock();
        json!({
            "id": a.id, "instance": a.instance_label(), "name": a.name, "role": a.role, "version": a.version,
            "port": a.port, "quic": a.quic, "ca": a.ca, "os": a.os, "arch": a.arch,
        })
    });
    json!({
        "authenticated": caller.is_some(),
        // Who (W10-R01): `admin` for the owner, `user:<name>`, a token, a job; and what they
        // hold, when that isn't everything.
        "principal": caller.map(|c| c.name.clone()),
        "kind": caller.map(|c| c.kind),
        "grants": caller.and_then(crate::authz::Caller::grants),
        "groups": groups,
        // The browser protocol's endpoint, when served (`crate::live`): a page never tries it on a
        // server that lacks it.
        "live": hub.live.offer(),
        // What it may do (W10-R02), for pages that show only what one can use: each
        // capability held on the whole server, or only somewhere (a repository, …).
        "can": caller.map(|c| {
            crate::authz::CAPABILITIES
                .iter()
                .filter_map(|cap| {
                    if c.can(cap, &crate::authz::Scope::Server) {
                        Some(((*cap).to_owned(), json!("server")))
                    } else {
                        c.has_somewhere(cap).then(|| ((*cap).to_owned(), json!("some")))
                    }
                })
                .collect::<serde_json::Map<String, serde_json::Value>>()
        }),
        "public_url": hub.cfg.server.public_url,
        // Where pages agents show are served, when not here (docs/previews.md).
        "pages_url": hub.cfg.server.pages_url,
        // Where else browsers find this server: its runners' front doors (HTTPS).
        "origins": hub.front_doors.origins(),
        "version": env!("CARGO_PKG_VERSION"),
        "hostname": crate::config::Config::hostname(),
        "discovery": announced,
    })
}

// ------------------------------------------------------------------ overview / system

async fn overview(hub: &Arc<Hub>, caller: &crate::authz::Caller) -> R {
    // A person of some repositories sees theirs (W10): their runs, jobs and queue, none of the
    // server's (its runners, processes and samples, its host's name).
    if let Some(repos) = crate::authz::gate::readable(hub, caller, "runs.read").await {
        let stats = hub.store.stats(Some(repos.clone())).await?;
        let queue = hub.store.queue_among(repos.clone()).await?;
        let system = json!({ "version": env!("CARGO_PKG_VERSION"), "uptime_s": hub.started.elapsed().as_secs(), "hostname": null, "generation": hub.generation, "children": [], "sample": null });
        return Ok(srv::json(StatusCode::OK, &json!({ "stats": stats, "queue": queue, "agents": null, "system": system, "sample": null, "narrowed": repos })));
    }
    // Kept encoded ahead of its asking (goal 10, plan 01a1a87b2dae step 2), made of the last day's
    // figures and the runners' rows kept in memory, the runners' live records and the server's own
    // figures: nothing is read from the store or encoded while it's answered. The store's, while
    // what's kept is behind it.
    hub.harness.views.start(hub);
    if let Some(made) = overview_key(hub).and_then(|key| hub.harness.views.overview.answer(key, |k| overview_made(hub, k)).ok()) {
        return Ok(pre_named(made));
    }
    let stats = hub.store.stats(None).await?;
    let agents = hub.agent_list().await;
    Ok(srv::json(StatusCode::OK, &overview_json(hub, &stats, &agents, &system_now(hub), queue_now())))
}

/// The jobs queued and running now, as the overview says them.
fn queue_now() -> (i64, i64) {
    let m = crate::metrics::global();
    (m.jobs_queued.get(), m.jobs_running.get())
}

/// The overview of every repository: the last day's figures `stats`, the runners `agents`, the server
/// as `system` says and the jobs `queue` (queued, running).
fn overview_json(hub: &Hub, stats: &serde_json::Value, agents: &[crate::hub::AgentLive], system: &crate::reads::SystemNow, queue: (i64, i64)) -> serde_json::Value {
    overview_of(hub, stats, RunnerCounts::of_list(agents), system, queue)
}

/// [`overview_json`] of the runners as `agents` counts them.
fn overview_of(hub: &Hub, stats: &serde_json::Value, agents: RunnerCounts, system: &crate::reads::SystemNow, queue: (i64, i64)) -> serde_json::Value {
    json!({
        "stats": stats,
        "queue": { "queued": queue.0, "running": queue.1 },
        "agents": {
            "connected": agents.connected,
            "total": agents.total,
            "slots": agents.slots,
            "busy": agents.busy,
        },
        "system": system_of(hub, system),
        "sample": system.sample,
    })
}

/// The runners as the overview says them: those connected, all of them, and the connected ones' slots and busy
/// slots.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RunnerCounts {
    connected: usize,
    total: usize,
    slots: u32,
    busy: u32,
}

impl RunnerCounts {
    /// One more runner, connected or not, with its slots and busy slots.
    fn add(&mut self, connected: bool, slots: u32, busy: u32) {
        self.total = self.total.saturating_add(1);
        if connected {
            self.connected = self.connected.saturating_add(1);
            self.slots = self.slots.wrapping_add(slots);
            self.busy = self.busy.wrapping_add(busy);
        }
    }

    /// The runners of a list (`Hub::agent_list`'s).
    fn of_list(agents: &[crate::hub::AgentLive]) -> Self {
        let mut n = Self::default();
        for a in agents {
            n.add(a.connected, a.slots, a.busy);
        }
        n
    }

    /// What `Hub::agent_list_of` lists of the store's `rows` and the live records `live`, counted with none of
    /// them copied (goal 10, plan 01a1a87b2dae step 5: the overview copied every runner's whole record, its facts
    /// with it, to count them, and `read /api/overview` held its shard 0.39 to 0.64 ms on the CPU in rt9's lab).
    /// As it lists them: each row as its live record says where it has one (by its name), else not connected; then
    /// each live record no listed runner is named as.
    fn of_rows(rows: &[crate::store::AgentRow], live: &HashMap<String, Arc<crate::hub::AgentLive>>) -> Self {
        let mut n = Self::default();
        for r in rows {
            match live.get(&r.name) {
                Some(l) => n.add(l.connected, l.slots, l.busy),
                None => n.add(false, r.slots, 0),
            }
        }
        // A listed runner is named as its live record names it, where it has one; so is a live record listed here.
        let named = |r: &crate::store::AgentRow, name: &str| live.get(&r.name).map_or(r.name.as_str(), |l| l.name.as_str()) == name;
        let mut listed: Vec<&str> = Vec::new();
        for (name, l) in live {
            if !rows.iter().any(|r| named(r, name)) && !listed.contains(&name.as_str()) {
                n.add(l.connected, l.slots, l.busy);
                listed.push(l.name.as_str());
            }
        }
        n
    }
}

/// What `GET /api/overview` is made of now, for whoever reads every repository's runs: none while the
/// last day's figures or the runners' rows kept in memory are behind the store's.
fn overview_key(hub: &Hub) -> Option<crate::reads::OverviewKey> {
    let v = &hub.harness.views;
    let stats = v.stats.fresh_while(&hub.store, crate::store::Stats::holds_now, crate::store::stats_now)?;
    let rows = v.agent_rows.fresh(&hub.store, crate::store::agent_rows)?;
    Some(crate::reads::OverviewKey { stats, rows, live: hub.agents.get(), system: system_now(hub), queue: queue_now() })
}

/// `GET /api/overview` made of what `key` says, encoded: its runners counted from their rows and live records as
/// they're kept, none copied ([`RunnerCounts::of_rows`]), so its making is the few kilobytes it encodes.
fn overview_made(hub: &Hub, key: &crate::reads::OverviewKey) -> anyhow::Result<Bytes> {
    let agents = RunnerCounts::of_rows(&key.rows.1.rows, &key.live);
    crate::reads::encode(&overview_of(hub, &key.stats.1.json, agents, &key.system, key.queue))
}

/// The maker's turn ([`crate::reads::Views::start`]): each read kept ahead that's been asked for, made
/// again where what it's made of changed: the runners' list on the compute pool ([`agents_making`]: each
/// runner's record copied and the list encoded, hundreds of microseconds for a long one, more than a shard's
/// quantum), the overview here (its counts and a few kilobytes encoded). Says when the overview next changes
/// by itself: at the next second of the server's uptime, which it says.
pub(crate) async fn make_ahead(hub: &Arc<Hub>) -> Option<Instant> {
    let v = &hub.harness.views;
    if v.agents.asked()
        && let Some(key) = agents_key(hub)
        && !v.agents.holds(&key)
        && let Err(e) = agents_making(hub, key).await
    {
        tracing::debug!(error = %format!("{e:#}"), "the runners' list wasn't made ahead: a read makes it");
    }
    if !v.overview.asked() {
        return None;
    }
    if let Some(key) = overview_key(hub)
        && !v.overview.holds(&key)
    {
        let was = v.overview.now();
        match overview_made(hub, &key) {
            Ok(made) => v.overview.keep(&was, key, crate::reads::Made::of(made)),
            Err(e) => tracing::debug!(error = %format!("{e:#}"), "the overview wasn't made ahead: a read makes it"),
        }
    }
    Some(hub.started + Duration::from_secs(hub.started.elapsed().as_secs().saturating_add(1)))
}

/// `GET /api/agents`: the runners `caller` may read. The owner's, kept encoded ahead of its asking
/// (goal 10, plan 01a1a87b2dae step 2), made of the runners' rows kept in memory and their live
/// records: nothing is read from the store or encoded while it's answered. One that finds it behind what
/// it's made of (a runner's record changed since its maker's turn) has it made on the compute pool, as its
/// maker does, nothing of it on the request's shard (step 5: made there, `read /api/agents` held its shard
/// 0.28 to 0.40 ms on the CPU in rt9's lab). The store's, while the rows kept are behind it.
async fn agents_read(hub: &Arc<Hub>, caller: &crate::authz::Caller) -> Response {
    if caller.is_owner() {
        hub.harness.views.start(hub);
        if let Some(key) = agents_key(hub) {
            if let Some(made) = hub.harness.views.agents.held(&key) {
                return pre_named(made);
            }
            match agents_making(hub, key).await {
                Ok(made) => return pre_named(made),
                Err(e) => tracing::debug!(error = %format!("{e:#}"), "the runners' list wasn't made off the shard: the store's answers"),
            }
        }
    }
    srv::json(StatusCode::OK, &crate::doctor::runners_for(hub, caller).await)
}

/// The owner's `GET /api/agents` as `key` says it, made on the compute pool (each runner's record copied, the list
/// encoded and named: `crate::reads::Made`) and kept for the reads after it, unless a making begun after this one's
/// was kept meanwhile (goal 10, plan 01a1a87b2dae step 5): what its maker makes, and a read that finds it behind.
async fn agents_making(hub: &Arc<Hub>, key: crate::reads::AgentsKey) -> anyhow::Result<crate::reads::Made> {
    let was = hub.harness.views.agents.now();
    let h = Arc::clone(hub);
    let (key, made) = crate::rt::compute(move || {
        let made = crate::reads::encode(&agents_made(&h, &key)).map(crate::reads::Made::of);
        (key, made)
    })
    .await?;
    let made = made?;
    hub.harness.views.agents.keep(&was, key, made.clone());
    Ok(made)
}

/// What the owner's `GET /api/agents` is made of now: none while the runners' rows kept in memory are
/// behind the store's, or while a machine runs more than one runner process (what each holds is read
/// from more than these: `Hub::runner_processes`).
fn agents_key(hub: &Hub) -> Option<crate::reads::AgentsKey> {
    let rows = hub.harness.views.agent_rows.fresh(&hub.store, crate::store::agent_rows)?;
    let (live, instances) = (hub.agents.get(), hub.instances.get());
    if !instances.crowded().is_empty() {
        return None;
    }
    let transports = live.values().filter(|a| a.connected).filter_map(|a| a.peer).map(|p| (p, hub.tunnels.transport(p))).collect();
    Some(crate::reads::AgentsKey { rows, live, instances, refusals: hub.refusals.get(), drops: crate::metrics::net().toward_all(), transports })
}

/// The runners as `key` says they are, as `Hub::agent_list` lists them.
fn agents_made(hub: &Hub, key: &crate::reads::AgentsKey) -> Vec<crate::hub::AgentLive> {
    let rows = &key.rows.1;
    hub.agent_list_of(rows.rows.clone(), &rows.labels, &rows.machines, &key.live)
}

/// What the server says of itself now ([`crate::reads::SystemNow`]).
fn system_now(hub: &Hub) -> crate::reads::SystemNow {
    crate::reads::SystemNow {
        uptime_s: hub.started.elapsed().as_secs(),
        hostname: crate::config::Config::hostname(),
        children: crate::server::registry().snapshot(),
        sample: hub.history.load().get("server").and_then(|q| q.back().cloned()),
        kubernetes: hub.k8s.get().is_some_and(Option::is_some),
    }
}

/// The server as `s` says it is, as `GET /api/system` and the overview say it.
fn system_of(hub: &Hub, s: &crate::reads::SystemNow) -> serde_json::Value {
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_s": s.uptime_s,
        "hostname": s.hostname,
        "generation": hub.generation,
        "children": s.children,
        "sample": s.sample,
        "kubernetes": s.kubernetes,
        "runtime": "hyper-rt",
        // The browser protocol's endpoint, when served (`crate::live`).
        "live": hub.live.offer(),
    })
}

fn system_json(hub: &Hub) -> serde_json::Value {
    system_of(hub, &system_now(hub))
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
    let h = hub.history.load();
    let mut out = serde_json::Map::new();
    for (k, v) in h.iter() {
        if q.get("agent").is_none_or(|a| a == k) {
            out.insert(k.clone(), json!(&**v));
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
    /// `native`: kiln hosts it (W14-R01); `upstream` (the default): kiln follows `clone_url`.
    mode: Option<String>,
    /// A native repository's history, copied from this URL (`kiln repo import`).
    import: Option<String>,
    /// A new, empty native repository's trunk (`main`).
    default_branch: Option<String>,
    /// A native repository's upstream, its trunk pushed there after each land (W14-R01).
    mirror: Option<String>,
    /// A native repository anyone may read over git, as a public GitHub repository.
    #[serde(default)]
    public: bool,
}

async fn add_repo(hub: &Hub, req: Request) -> R {
    let r: NewRepo = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    let name = r.name.trim().trim_start_matches("https://github.com/").trim_end_matches(".git").to_owned();
    if name.split('/').count() != 2 || name.contains(char::is_whitespace) {
        return Err(err(StatusCode::BAD_REQUEST, "name must be owner/repo"));
    }
    let native = match r.mode.as_deref() {
        None => r.import.is_some(),
        Some("native") => true,
        Some("upstream") => false,
        Some(m) => return Err(err(StatusCode::BAD_REQUEST, format!("mode `{m}`: `native` or `upstream`"))),
    };
    let default_branch = r.default_branch.clone().filter(|_| native).or_else(|| native.then(|| "main".to_owned()));
    if let Some(m) = r.mirror.as_deref() {
        if !native {
            return Err(err(StatusCode::BAD_REQUEST, "a mirror is a native repository's upstream"));
        }
        crate::ci::hosting::importable(m).map_err(|e| err(StatusCode::BAD_REQUEST, e.replace("an import copies", "a mirror is")))?;
    }
    let clone_url = if native {
        if let Some(url) = &r.import {
            crate::ci::hosting::importable(url).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
        }
        let path = crate::ci::hosting::create(hub, &name, default_branch.as_deref().unwrap_or("main"), r.import.as_deref()).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
        crate::ci::hosting::clone_url(&path)
    } else {
        r.clone_url.unwrap_or_else(|| format!("https://github.com/{name}.git"))
    };
    // A new repository kiln hosts requires signed commits as the server says (W27-R03); one added
    // again keeps what it had.
    let new = native && !hub.store.repos().await?.iter().any(|x| x.name == name);
    let repo = hub
        .store
        .upsert_repo(Repo {
            id: crate::util::new_id(),
            clone_url,
            name: name.clone(),
            branches: r.branches,
            poll_seconds: r.poll_seconds.unwrap_or(hub.cfg.gitops.poll_seconds),
            enabled: true,
            created_at: crate::util::now_unix(),
            last_polled_at: None,
            last_error: None,
            mode: if native { "native" } else { "upstream" }.into(),
            default_branch,
            mirror: r.mirror.clone(),
        })
        .await?;
    if native {
        hub.store.set_repo_public(repo.id.clone(), r.public).await?;
    }
    if new {
        hub.store.set_repo_signatures(repo.id.clone(), hub.cfg.ci.signatures == kiln_ci::land::SignatureMode::Required).await?;
    }
    hub.store.audit("admin".into(), "repo.add".into(), name).await;
    crate::gitops::want(hub, &repo.id);
    let mut body = serde_json::to_value(&repo).unwrap_or_default();
    if let Some(o) = body.as_object_mut() {
        o.insert("public".into(), json!(native && r.public));
    }
    Ok(srv::json(StatusCode::OK, &body))
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
    hub.store.delete_repo(repo.id.clone()).await?;
    // Its runs go with it: the CI engine stops their legs where they run, as GitHub does.
    hub.ci.send(crate::ci::dispatch::Msg::RepoDeleted { repo: repo.id });
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

/// `GET /api/runs`: runs newest first, of the repositories the caller may read (W10).
async fn runs(hub: &Hub, q: &HashMap<String, String>, caller: &crate::authz::Caller) -> R {
    let among = crate::authz::gate::readable(hub, caller, "runs.read").await;
    // Every repository's newest, asked with nothing in the query: kept in memory, encoded once (goal 10,
    // plan 01a1a87b2dae step 2), while it's what the store holds.
    if among.is_none()
        && q.is_empty()
        && let Some(listed) = hub.harness.views.runs.fresh(&hub.store, crate::reads::runs_listed)
    {
        return Ok(pre_encoded(listed.1.clone()));
    }
    Ok(srv::json(StatusCode::OK, &crate::reads::runs(&hub.store, q, among).await?))
}

async fn run(hub: &Hub, id: &str, q: &HashMap<String, String>) -> R {
    let run = crate::reads::run(&hub.store, id, q).await?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such run"))?;
    Ok(srv::json(StatusCode::OK, &run))
}

/// `POST /api/runs/:id/cancel`: the run; with `{ jobs: [ids] }` (docs/ci.md W26-R21), just those
/// of its jobs, the run going on, each answering what became of it.
async fn cancel(hub: &Hub, id: &str, req: Request) -> R {
    let by = req.extensions().get::<crate::authz::Caller>().map_or_else(|| "admin".to_owned(), |c| c.name.clone());
    let jobs = srv::body_json::<SetReq>(req.into_body(), JSON_LIMIT).await.ok().and_then(|b| b.jobs).filter(|j| !j.is_empty());
    if let Some(jobs) = jobs {
        let v = crate::ci::retry::cancel_set(hub, id, jobs, &by).await.map_err(|(c, m)| err(c, m))?;
        return Ok(srv::json(StatusCode::OK, &v));
    }
    hub.cancel_run(id, "cancelled by user").await?;
    hub.store.audit("admin".into(), "run.cancel".into(), id.to_owned()).await;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

/// `POST /api/runs/:id/pause` and `/resume` with `{ jobs: [ids] }` (docs/ci.md W26-R22): those of
/// the run's jobs, either engine's, paused (frozen where they run, or held before they start) or
/// resumed, each answering what became of it.
async fn pause(hub: &Hub, id: &str, req: Request, paused: bool) -> R {
    let by = req.extensions().get::<crate::authz::Caller>().map_or_else(|| "admin".to_owned(), |c| c.name.clone());
    let jobs = srv::body_json::<SetReq>(req.into_body(), JSON_LIMIT).await.ok().and_then(|b| b.jobs).filter(|j| !j.is_empty());
    let Some(jobs) = jobs else { return Err(err(StatusCode::BAD_REQUEST, "name the jobs: { jobs: [ids] }")) };
    let v = crate::ci::retry::pause_set(hub, id, jobs, paused, &by).await.map_err(|(c, m)| err(c, m))?;
    Ok(srv::json(StatusCode::OK, &v))
}

#[derive(Deserialize)]
struct RetryReq {
    #[serde(default = "yes")]
    only_failed: bool,
    /// Just these jobs (docs/ci.md W26-R21), passed ones too.
    #[serde(default)]
    jobs: Option<Vec<String>>,
}

/// A body naming some of a run's jobs (docs/ci.md W26-R21).
#[derive(Deserialize)]
struct SetReq {
    #[serde(default)]
    jobs: Option<Vec<String>>,
}

fn yes() -> bool {
    true
}

async fn retry(hub: &Hub, id: &str, req: Request) -> R {
    let by = req.extensions().get::<crate::authz::Caller>().map_or_else(|| "admin".to_owned(), |c| c.name.clone());
    let body = srv::body_json::<RetryReq>(req.into_body(), JSON_LIMIT).await.ok();
    if let Some(jobs) = body.as_ref().and_then(|b| b.jobs.clone()).filter(|j| !j.is_empty()) {
        let v = crate::ci::retry::retry_set(hub, id, &jobs, &by).await.map_err(|(c, m)| err(c, m))?;
        return Ok(srv::json(StatusCode::OK, &v));
    }
    let only_failed = body.is_none_or(|r| r.only_failed);
    let n = hub.store.retry_run(id.to_owned(), only_failed).await?;
    hub.emit_run(id).await;
    hub.wake.notify();
    hub.store.audit("admin".into(), "run.retry".into(), id.to_owned()).await;
    Ok(srv::json(StatusCode::OK, &json!({ "jobs": n })))
}

async fn job(hub: &Hub, id: &str) -> R {
    let j = crate::reads::job(&hub.store, id).await?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such job"))?;
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

/// When each piece of a step's log arrived (`{ times: [[offset, unix_ms], ...], size }`, oldest
/// first, and how long the log was as they were read: the lines they cover), for the times Ship's
/// Output puts beside its lines (plan 01a1856fe81375b1bd1d1d86f270a45b, step 9); a CI leg a rerun
/// kept answers with its original's, as its log does.
async fn job_log_times(hub: &Hub, id: &str, q: &HashMap<String, String>) -> R {
    let step = q.get("step").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut of = id.to_owned();
    if hub.logs.size(&of, step).await == 0
        && let Some(from) = hub.store.reused_from(id.to_owned()).await?
    {
        of = from;
    }
    let size = hub.logs.size(&of, step).await;
    let times = hub.logs.times(&of, step).await;
    Ok(srv::json(StatusCode::OK, &json!({ "times": times, "size": size })))
}

fn job_logs_ws(hub: Arc<Hub>, job: String, step: u32, req: Request) -> R {
    let sock = ui::Socket::of(Wire::LogUp, Wire::LogDown, &caller_name(&req), None);
    let (resp, upgrade) = ws::accept_commands(req).map_err(|r| ApiError(r.status, r.reason.into()))?;
    crate::rt::spawn(async move {
        match upgrade.await {
            Ok(socket) => {
                if let Err(e) = stream_logs(hub, job, step, socket, &sock).await {
                    tracing::debug!(error = %e, "log stream ended");
                }
            }
            Err(e) => tracing::debug!(error = %e, "log stream upgrade failed"),
        }
    });
    Ok(resp)
}

async fn stream_logs(hub: Arc<Hub>, job: String, step: u32, socket: ws::WebSocket, sock: &ui::Socket) -> anyhow::Result<()> {
    use futures::FutureExt as _;
    let (mut tx, mut rx_ws) = socket.split();
    let live = hub.logs.is_live(&job);
    let mut rx = live.then(|| hub.logs.subscribe(&job));
    let backlog = hub.logs.read(&job, step).await;
    for chunk in backlog.chunks(64 * 1024) {
        tx.send(ws::binary(Bytes::copy_from_slice(chunk))).await?;
    }
    let Some(rx) = rx.as_mut() else {
        tx.send(ws::text(LogDown::End.text())).await?;
        tx.close(None).await.ok();
        return Ok(());
    };
    loop {
        futures::select_biased! {
            ev = rx.recv().fuse() => match ev {
                Ok(LogEvent::Data { idx, data }) if idx == step => tx.send(ws::binary(data)).await?,
                Ok(LogEvent::Data { .. }) => {}
                Ok(LogEvent::Step { idx, status }) => {
                    tx.send(ws::text(LogDown::Step { idx, status: status.to_owned() }.text())).await?;
                }
                Ok(LogEvent::End) | Err(async_broadcast::RecvError::Closed) => {
                    tx.send(ws::text(LogDown::End.text())).await?;
                    tx.close(None).await.ok();
                    return Ok(());
                }
                Err(async_broadcast::RecvError::Overflowed(n)) => {
                    let note = format!("\r\n\x1b[33m[kiln] viewer fell behind; {n} chunks skipped (reload for the full log)\x1b[0m\r\n");
                    tx.send(ws::binary(note.into_bytes())).await?;
                }
            },
            msg = rx_ws.next().fuse() => {
                if hub.unread_only(sock, &mut tx, ui::came(msg), |r| LogDown::Refused(r).text()).await? {
                    return Ok(());
                }
            }
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

/// The most one piece of an upload in pieces may be.
const ARTIFACT_PIECE: usize = 64 << 20;

/// A query's `name`, as a number.
fn query_u64(req: &Request, name: &str) -> Option<u64> {
    req.uri().query()?.split('&').find_map(|kv| kv.strip_prefix(name)?.strip_prefix('='))?.parse().ok()
}

/// `PUT /api/jobs/:id/artifacts/upload`: a job's artifacts (a tar), whole; or, with `?at=n`, the
/// piece of them at byte `n` (`put_artifacts_piece`).
async fn put_artifacts(hub: &Hub, id: &str, req: Request) -> R {
    if !job_token_ok(hub, req.headers(), id) {
        return Err(err(StatusCode::UNAUTHORIZED, "bad job token"));
    }
    if let Some(at) = query_u64(&req, "at") {
        return put_artifacts_piece(hub, id, at, req).await;
    }
    let path = artifact_path(hub, id);
    // Kept only once all of it came: an upload that broke off leaves nothing a release would take
    // its binaries from (a tar cut short reads as shorter files, with no error).
    let part = path.with_extension("part");
    let (sink, done) = crate::rt::file_sink(part.clone(), 8 << 30)?;
    let mut body = req.into_body();
    let mut broke = None;
    loop {
        match srv::next_chunk(&mut body).await {
            Ok(Some(chunk)) => {
                if sink.send(chunk).await.is_err() {
                    break; // the writer failed; its error is reported below
                }
            }
            Ok(None) => break,
            Err(e) => {
                broke = Some(e.to_string());
                break;
            }
        }
    }
    drop(sink);
    let written = done.await;
    if let Some(e) = broke {
        let _ = std::fs::remove_file(&part);
        return Err(err(StatusCode::BAD_REQUEST, format!("the upload broke off: {e}")));
    }
    if let Err(e) = written {
        let _ = std::fs::remove_file(&part);
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")));
    }
    std::fs::rename(&part, &path).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("keep {}: {e}", path.display())))?;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

/// How long a piece of a job's artifacts may go without a byte before what came of it is kept and
/// answered for: longer than any pause of an upload still going, well inside the 120 s a runner
/// gives a piece. A runner's HTTP client could stop up to 64 KiB short of a piece's end and wait
/// for an answer (fixed in kiln-http's client); answered, it goes on from where the pieces end.
const PIECE_STALL: Duration = Duration::from_secs(15);

/// One piece of a job's artifacts, at byte `at`: taken only where the pieces so far end (the first
/// starts them afresh), so a piece that broke off costs only itself. One elsewhere is refused with
/// where they end (`409 {"have": n}`), for the upload to go on from there. What came of a piece
/// that stalls or breaks off is kept too, and answered for at once, so the runner sends only the
/// rest, and nothing of it is held while a connection waits.
async fn put_artifacts_piece(hub: &Hub, id: &str, at: u64, req: Request) -> R {
    let part = artifact_path(hub, id).with_extension("part");
    let mut body = req.into_body();
    let mut came = bytes::BytesMut::new();
    let whole = loop {
        match crate::rt::timeout(PIECE_STALL, srv::next_chunk(&mut body)).await {
            Ok(Ok(Some(chunk))) => {
                if came.len().saturating_add(chunk.len()) > ARTIFACT_PIECE {
                    return Err(err(StatusCode::PAYLOAD_TOO_LARGE, format!("a piece is at most {ARTIFACT_PIECE} bytes")));
                }
                came.extend_from_slice(&chunk);
            }
            Ok(Ok(None)) => break true,
            Ok(Err(_)) | Err(_) => break false,
        }
    };
    drop(body);
    let piece = came.freeze();
    let had = crate::rt::blocking(move || -> anyhow::Result<Result<u64, u64>> {
        if let Some(dir) = part.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if at == 0 {
            let _ = std::fs::remove_file(&part);
        }
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&part)?;
        let len = f.metadata()?.len();
        if len != at {
            return Ok(Err(len));
        }
        std::io::Write::write_all(&mut f, &piece)?;
        Ok(Ok(len.saturating_add(u64::try_from(piece.len()).unwrap_or(u64::MAX))))
    })
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let answer = match had {
        Ok(have) => srv::json(StatusCode::OK, &serde_json::json!({"have": have})),
        Err(have) => srv::json(StatusCode::CONFLICT, &serde_json::json!({"have": have})),
    };
    // The rest of a piece that didn't all come is left unread: the connection ends with the answer,
    // and the runner doesn't send its next piece on it.
    Ok(if whole { answer } else { srv::with_header(answer, header::CONNECTION, "close") })
}

/// `GET /api/jobs/:id/artifacts/upload`: how much of an upload in pieces has come (`{"have": n}`;
/// a server that answers so takes them in pieces).
async fn artifacts_had(hub: &Hub, id: &str, headers: &HeaderMap) -> R {
    if !job_token_ok(hub, headers, id) {
        return Err(err(StatusCode::UNAUTHORIZED, "bad job token"));
    }
    let part = artifact_path(hub, id).with_extension("part");
    let have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    Ok(srv::json(StatusCode::OK, &serde_json::json!({"have": have})))
}

/// `POST /api/jobs/:id/artifacts/upload?size=n&sha256=hex`: the pieces' end. They're the job's
/// artifacts only if they come to its size and digest; else they're let go of (`409`), and nothing
/// a release would take its binaries from is kept.
async fn artifacts_done(hub: &Hub, id: &str, req: Request) -> R {
    if !job_token_ok(hub, req.headers(), id) {
        return Err(err(StatusCode::UNAUTHORIZED, "bad job token"));
    }
    let size = query_u64(&req, "size").ok_or_else(|| err(StatusCode::BAD_REQUEST, "how big? (`size`)"))?;
    let want = req
        .uri()
        .query()
        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("sha256=")))
        .map(str::to_ascii_lowercase)
        .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "its digest? (`sha256`, hex)"))?;
    let path = artifact_path(hub, id);
    let part = path.with_extension("part");
    let checked = crate::rt::blocking(move || -> anyhow::Result<Result<(), String>> {
        use anyhow::Context as _;
        use sha2::Digest as _;
        use std::io::Read as _;
        let mut f = std::fs::File::open(&part).context("no pieces came")?;
        let len = f.metadata()?.len();
        let mut h = sha2::Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            h.update(buf.get(..n).unwrap_or_default());
        }
        let got = hex::encode(h.finalize());
        if len != size || got != want {
            let _ = std::fs::remove_file(&part);
            return Ok(Err(format!("the pieces came to {len} bytes, digest {got}; the upload was {size} bytes, digest {want}")));
        }
        std::fs::rename(&part, &path).with_context(|| format!("keep {}", path.display()))?;
        Ok(Ok(()))
    })
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    checked.map_err(|why| err(StatusCode::CONFLICT, why))?;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

async fn get_artifacts(hub: &Hub, id: &str) -> R {
    let path = artifact_path(hub, id);
    let body = crate::rt::file_source(path).map_err(|_| err(StatusCode::NOT_FOUND, "no artifacts"))?;
    let r = srv::response(StatusCode::OK, "application/x-tar", srv::channel_body(body));
    Ok(srv::with_header(r, header::CONTENT_DISPOSITION, &format!("attachment; filename=\"artifacts-{id}.tar\"")))
}

// ------------------------------------------------------------------ agents

async fn forget_agent(hub: &Hub, name: &str, by: &str) -> R {
    let name = &hub.machine_of(name);
    hub.forget_agent(name, by).await?;
    hub.store.audit(by.to_owned(), "agent.forget".into(), name.to_owned()).await;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

/// `POST /api/me/identity` `{csr}`: the caller's certificate as a person, for the CSR's key, made
/// on their device (docs/ci.md W27-R02); asked again to renew it while they stay signed in. A
/// refusal says why, with a `code` the CLI acts on.
async fn me_identity(hub: &Hub, caller: crate::authz::Caller, req: Request) -> R {
    let b: serde_json::Value = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    let csr = b.get("csr").and_then(serde_json::Value::as_str).ok_or_else(|| err(StatusCode::BAD_REQUEST, "send a csr"))?;
    match crate::identities::issue_person(hub, &caller, csr).await {
        Ok(i) => {
            hub.store.audit(caller.name.clone(), "identity.person".into(), i.uri.clone()).await;
            Ok(srv::json(StatusCode::OK, &json!({ "uri": i.uri, "identity": i.identity, "cert_pem": i.cert_pem, "not_before": i.not_before, "not_after": i.not_after })))
        }
        Err(crate::identities::PersonRefused::Failed(e)) => Err(e.into()),
        Err(r) => Ok(srv::json(r.status(), &json!({ "error": r.to_string(), "code": r.code() }))),
    }
}

/// `POST /api/custody`: a signed custody record or delegation (its Sigstore bundle), taken from
/// `by` (W27-R08): kept and its hop checked, or refused and recorded, saying why.
async fn custody_take(hub: &Hub, req: Request, by: &str) -> R {
    let bundle: serde_json::Value = boxed(|| srv::body_json(req.into_body(), JSON_LIMIT)).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    let taken = crate::custody::take(&hub.store, &hub.pki.ca_der, &hub.pki.organization(), &bundle, &format!("api:{by}"), crate::util::now_unix())
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    match taken {
        Ok(t) => Ok(srv::json(if t.again { StatusCode::OK } else { StatusCode::CREATED }, &json!({ "digest": t.digest, "again": t.again, "broke": t.broke }))),
        Err(refused) => Err(err(StatusCode::UNPROCESSABLE_ENTITY, refused.to_string())),
    }
}

/// `GET /api/me/ssh-signers`: the SSH keys bound to the caller, a person, so the commits each signs
/// verify as them (docs/ci.md W27-R03); `POST` `{key}` binds one (its `.pub` file's line), and
/// `DELETE ?fingerprint=` unbinds one. Never an agent's, a job's or a peer's to change. A refusal
/// says why, with a `code`.
async fn me_ssh_signers(hub: &Hub, caller: crate::authz::Caller, req: Request) -> R {
    use crate::identities::PersonRefused;
    let refused = |r: PersonRefused| -> R {
        match r {
            PersonRefused::Failed(e) => Err(e.into()),
            r => Ok(srv::json(r.status(), &json!({ "error": r.to_string(), "code": r.code() }))),
        }
    };
    match req.method().clone() {
        Method::POST => {
            let b: serde_json::Value = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
            let key = b.get("key").and_then(serde_json::Value::as_str).ok_or_else(|| err(StatusCode::BAD_REQUEST, "send a key: an SSH public key, as its .pub file has it"))?;
            match crate::identities::add_ssh_signer(&hub.store, &caller, key).await {
                Ok(signer) => {
                    hub.store.audit(caller.name.clone(), "identity.ssh_signer".into(), format!("bound {} ({})", signer.fingerprint, signer.kind)).await;
                    Ok(srv::json(StatusCode::OK, &json!({ "signer": signer })))
                }
                Err(r) => refused(r),
            }
        }
        Method::DELETE => {
            let fingerprint = srv::query(&req).remove("fingerprint").ok_or_else(|| err(StatusCode::BAD_REQUEST, "say which key: ?fingerprint=SHA256:…"))?;
            match crate::identities::remove_ssh_signer(&hub.store, &caller, &fingerprint).await {
                Ok(removed) => {
                    if removed {
                        hub.store.audit(caller.name.clone(), "identity.ssh_signer".into(), format!("unbound {fingerprint}")).await;
                    }
                    Ok(srv::json(StatusCode::OK, &json!({ "removed": removed })))
                }
                Err(r) => refused(r),
            }
        }
        _ => match crate::identities::ssh_signers(&hub.store, &caller).await {
            Ok(signers) => Ok(srv::json(StatusCode::OK, &json!({ "signers": signers }))),
            Err(r) => refused(r),
        },
    }
}

/// `GET /api/identities/revocations`: every revocation, oldest first (W27-R01).
async fn identity_revocations(hub: &Hub) -> R {
    let revs = crate::identities::revocations(&hub.store).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let rows: Vec<serde_json::Value> = revs.iter().map(|r| json!({ "serial": r.serial, "key": r.key, "at": r.at, "reason": r.reason, "by": r.by })).collect();
    Ok(srv::json(StatusCode::OK, &json!({ "revocations": rows })))
}

#[derive(Deserialize)]
struct RevokeReq {
    serial: Option<String>,
    key: Option<String>,
    subject: Option<String>,
    reason: String,
}

/// `POST /api/identities/revoke` `{serial | key | subject, reason}`: revoked from now on, refused
/// at once by the TLS directory (W27-R01).
async fn identity_revoke(hub: &Hub, req: Request, by: &str) -> R {
    use crate::identities::Target;
    let r: RevokeReq = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    let target = match (r.serial, r.key, r.subject) {
        (Some(s), None, None) => Target::Serial(s),
        (None, Some(k), None) => Target::Key(k),
        (None, None, Some(s)) => Target::Subject(s),
        _ => return Err(err(StatusCode::BAD_REQUEST, "name exactly one of serial, key or subject")),
    };
    if r.reason.trim().is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "say why it's revoked (reason)"));
    }
    let at = crate::util::now_unix();
    let keys = crate::identities::revoke(&hub.store, target.clone(), r.reason.trim(), by, at).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    if !keys.is_empty() {
        hub.net.command(crate::net::Out::RevokeKeys { keys: keys.clone(), why: crate::identities::refusal(at, r.reason.trim(), by) }).await;
    }
    hub.store.audit(by.to_owned(), "identity.revoke".into(), format!("{target:?}: {}", r.reason.trim())).await;
    // The server's or CD's own, revoked, is issued anew at once (a revoked key replaced).
    if let Err(e) = crate::identities::own_identities(&hub.store, &hub.pki).await {
        tracing::warn!(error = %format!("{e:#}"), "the server's and CD's identities weren't issued anew");
    }
    Ok(srv::json(StatusCode::OK, &json!({ "at": at, "keys": keys })))
}

#[derive(Deserialize)]
struct DrainReq {
    draining: bool,
}

#[derive(Deserialize)]
struct LabelReq {
    label: String,
}

/// `POST /api/agents/:name/label` (docs/machines.md): the name people see for runner `name`, which
/// kiln goes on knowing by `name`. A name another runner has, as its own or as its label, is
/// refused: what people type means one machine.
async fn label_agent(hub: &Hub, name: &str, req: Request) -> R {
    let want: LabelReq = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    let label = crate::pki::sanitize_name(want.label.trim());
    if want.label.trim().is_empty() || crate::pki::reserved(&label) {
        return Err(err(StatusCode::BAD_REQUEST, format!("{:?} can't be a runner's name", want.label)));
    }
    let agents = hub.agent_list().await;
    let Some(me) = agents.iter().find(|a| a.name == name) else {
        return Err(err(StatusCode::NOT_FOUND, format!("no runner `{name}`")));
    };
    if let Some(other) = agents.iter().find(|a| a.name != name && (a.name == label || a.label.as_deref() == Some(label.as_str()))) {
        return Err(err(StatusCode::CONFLICT, format!("{label} is another runner's name ({})", shown(&other.name, other.label.as_deref()))));
    }
    hub.set_label(name, &label, &me.hostname).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    hub.store.audit("admin".into(), "agent.label".into(), format!("{} is shown as {label}", shown(name, me.label.as_deref()))).await;
    Ok(srv::json(StatusCode::OK, &json!({ "name": name, "label": label })))
}

async fn drain_agent(hub: &Hub, name: &str, req: Request) -> R {
    let d: DrainReq = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    if hub.set_draining(name, d.draining).await {
        Ok(srv::status(StatusCode::NO_CONTENT))
    } else {
        Err(err(StatusCode::NOT_FOUND, "agent not connected"))
    }
}

/// `POST /api/agents/:name/probe` (docs/ci.md W13-R01, on request): the runner probes its facts
/// again now. The answer is its new report once it comes (within 30 s), or, when it hasn't yet,
/// 202 without one.
pub async fn probe_agent(hub: &Hub, name: &str) -> Result<Response, crate::ci::Fail> {
    let Some(before) = hub.agents.load().get(name).map(|a| a.facts.as_ref().map_or(0, |f| f.probed_at)) else {
        return Err((StatusCode::NOT_FOUND, format!("no runner `{name}`")));
    };
    if !hub.send_to_agent(name, crate::proto::LANE_CONTROL, &crate::proto::ServerMsg::ProbeFacts).await {
        return Err((StatusCode::CONFLICT, format!("{} isn't connected", hub.shown(name))));
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let fresh = hub.agents.load().get(name).and_then(|a| a.facts.as_ref().filter(|f| f.probed_at > before).map(|f| serde_json::to_value(f.as_ref()).unwrap_or_default()));
        if let Some(facts) = fresh {
            return Ok(srv::json(StatusCode::OK, &json!({ "runner": name, "facts": facts })));
        }
        if std::time::Instant::now() >= deadline {
            return Ok(srv::json(StatusCode::ACCEPTED, &json!({ "runner": name, "facts": null, "note": "asked: its report hasn't come yet" })));
        }
        crate::rt::sleep(std::time::Duration::from_millis(250)).await;
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
            "command": format!("curl -fsSL {url}/install.sh | sh -s -- --token={token} --labels baremetal,bench"),
            "macos": format!("curl -fsSL {url}/install.sh | sh -s -- --token={token} --labels macos"),
            "windows": format!("& ([scriptblock]::Create((irm '{url}/install.ps1'))) -Token '{token}' -Labels 'windows'"),
            "manual": format!("kiln agent --server {url} --token={token} --labels baremetal,bench"),
            // No token: the machine asks to join and an admin approves it (verification code).
            "join": format!("curl -fsSL {url}/install.sh | sh -s -- --join"),
            "join_manual": format!("kiln join --server {url}"),
            // Windows: install.ps1 without a token asks to join the same way.
            "join_windows": format!("irm '{url}/install.ps1' | iex"),
            // One line per OS that downloads kiln from this server and installs the agent with the
            // join token (`install.sh` passes `--token`/`--labels` to `kiln install --agent`).
            "install": {
                "linux": format!("curl -fsSL {url}/install.sh | sh -s -- --token={token} --labels baremetal,linux"),
                "macos": format!("curl -fsSL {url}/install.sh | sh -s -- --token={token} --labels baremetal,macos"),
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
                "linux": format!("curl -fsSL {url}/install.sh | sh -s -- --outpost --token={token}"),
                "macos": format!("curl -fsSL {url}/install.sh | sh -s -- --outpost --token={token}"),
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
    // A `k8s` pool's executor pod (docs/ci.md W13-R07): its runner is the pod's name and that
    // executor's, whatever it asks to be.
    let pod = if persistent { None } else { hub.executor_tokens.update(|t| t.remove(&r.token)).filter(|(_, _, exp)| *exp > std::time::Instant::now()) };
    let ephemeral = pod.is_some() || (!persistent && hub.ephemeral_tokens.update(|t| t.remove(&r.token)).is_some());
    let outpost = !persistent && !ephemeral && hub.outpost_invites.take(&r.token);
    if !persistent && !ephemeral && !outpost {
        crate::rt::sleep(Duration::from_millis(500)).await;
        return Err(err(StatusCode::UNAUTHORIZED, "invalid join token"));
    }
    let name = match &pod {
        Some((executor, pod_name, _)) => {
            hub.pod_executors.update(|p| p.insert(pod_name.clone(), executor.clone()));
            pod_name.clone()
        }
        None => crate::pki::sanitize_name(&r.name),
    };
    // A machine enrolling with the join token, or an outpost, is named as a joining one is: known
    // by its machine id when it sent one, shown by the name it asked for unless another machine
    // has it. A job's pod has its pod's name.
    let (name, label) = if persistent || outpost {
        // Registered by its machine id, or not at all (docs/machines.md, "One record a machine").
        if crate::join::machine_key(r.machine_id.as_deref()).is_none() {
            return Err(err(StatusCode::BAD_REQUEST, crate::join::NO_MACHINE_ID));
        }
        let (name, label) = name_and_label(hub, &name, &r.hostname, crate::join::clean_machine_id(r.machine_id.as_deref()).as_deref()).await?;
        if let Some(why) = a_copy_of_a_connected_machine(hub, &name) {
            return Err(err(StatusCode::CONFLICT, why));
        }
        let label = (label != name).then_some(label);
        (name, label)
    } else {
        (name, None)
    };
    if let Some(l) = &label
        && let Err(e) = hub.set_label(&name, l, &r.hostname).await
    {
        tracing::warn!(agent = %name, label = %l, error = %format!("{e:#}"), "couldn't keep the enrolling machine's label");
    }
    let days = if ephemeral {
        crate::pki::EPHEMERAL_DAYS
    } else if outpost {
        crate::pki::OUTPOST_DAYS
    } else {
        365
    };
    let cert = hub.pki.sign_csr(&r.csr, &name, days).map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    crate::identities::record(&hub.store, "machine", &name, &cert).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    // Who enrolled it (W27-R01): the owner, whose join token or invite it brought; kiln for its
    // own pods.
    let by = if ephemeral { "kiln" } else { "admin" };
    crate::identities::enrolled(&hub.store, &name, by).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    // A name forgotten earlier in this server's life is welcome again, as an approved join makes
    // it: its new key works, and its revoked keys stay refused (W27-R01).
    hub.net.command(crate::net::Out::Unrevoke { name: name.clone() }).await;
    let quic = if ephemeral {
        hub.cfg.kubernetes.server_quic.clone().unwrap_or_else(|| hub.cfg.server.quic_advertise.clone())
    } else {
        hub.cfg.server.quic_advertise.clone()
    };
    if persistent || outpost {
        let what = if outpost { "outpost.enroll" } else { "agent.enroll" };
        hub.store.audit(name.clone(), what.into(), shown(&name, label.as_deref())).await;
        tracing::info!(agent = %name, label = label.as_deref().unwrap_or(""), outpost, "agent enrolled");
    }
    Ok(srv::json(StatusCode::OK, &EnrollResponse { cert_pem: cert, ca_pem: hub.pki.ca_pem.clone(), quic, name, label }))
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
    // A process of a machine is the machine's, by its id (docs/machines.md, "One record a
    // machine"): one whose certificate carries a name from before gets its certificate for the id.
    let id = hub.machine_of(&name);
    if !crate::join::is_machine_key(&id) {
        return Err(err(StatusCode::FORBIDDEN, format!("{name} never said its machine id, so kiln can't tell which machine this process is of: start its runner once so it says, or {}", crate::join::NO_MACHINE_ID)));
    }
    let (name, cert) = if id == name {
        (name, cert)
    } else {
        match hub.pki.sign_moved(&r.cert_pem, &r.csr, &id) {
            Ok((_, cert)) => (id, cert),
            Err(e) => return Err(err(StatusCode::FORBIDDEN, format!("{e:#}"))),
        }
    };
    // Only machines this server still knows: a forgotten (revoked) one must enroll again.
    let known = hub.store.agents().await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?.iter().any(|a| a.name == name);
    if !known {
        return Err(err(StatusCode::FORBIDDEN, format!("{name} is not enrolled here any more; enroll it again")));
    }
    crate::identities::record(&hub.store, "machine", &name, &cert).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    hub.store.audit(name.clone(), "agent.instance".into(), String::new()).await;
    tracing::info!(agent = %name, "issued a certificate for a second runner process");
    Ok(srv::json(
        StatusCode::OK,
        &crate::agent::EnrollResponse { cert_pem: cert, ca_pem: hub.pki.ca_pem.clone(), quic: hub.cfg.server.quic_advertise.clone(), name, label: None },
    ))
}

/// `POST /api/agents/rekey` (docs/machines.md): a runner known by its name moves to its machine id.
/// Needs no token: the request carries the machine's current certificate and a CSR signed with its
/// key, as `/api/agents/instance`'s does ([`crate::pki::Pki::sign_moved`]). What kiln kept under its
/// name goes to the id ([`Hub::move_runner`]), its name becomes its label, and the answer carries
/// its certificate for the id. Refused for a machine this server doesn't know, for an id other than
/// the one its facts said, for an id a connected runner is known by or a connected record of the
/// machine says it is, and while a process of it is connected (what it holds would move from under
/// it). An id known already, not connected, is the same machine's second record: the two merge,
/// and every other record of the machine not connected folds in with them ([`Hub::fold_duplicates`]).
async fn rekey_agent(hub: &Hub, req: Request) -> R {
    let r: crate::agent::RekeyRequest = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    let Some(to) = crate::join::machine_key(Some(&r.machine_id)) else {
        return Err(err(StatusCode::BAD_REQUEST, "that isn't a machine id kiln makes"));
    };
    let (from, cert) = match hub.pki.sign_moved(&r.cert_pem, &r.csr, &to) {
        Ok(v) => v,
        Err(e) => {
            crate::rt::sleep(Duration::from_millis(500)).await;
            return Err(err(StatusCode::FORBIDDEN, format!("{e:#}")));
        }
    };
    if from == to {
        return Err(err(StatusCode::CONFLICT, format!("{to} is known by its machine id already")));
    }
    // A revoked key moves nowhere (W27-R01): its certificate is still signed, but kiln refuses it.
    let key = crate::pki::fingerprints(&r.cert_pem).map(|f| f.key).map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let revoked = crate::identities::revoked_keys(&hub.store).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    if let Some((_, why)) = revoked.iter().find(|(k, _)| *k == key) {
        crate::rt::sleep(Duration::from_millis(500)).await;
        return Err(err(StatusCode::FORBIDDEN, format!("{from}'s key was {why}")));
    }
    // Known by the id already (its name is one its certificate carried from before, moved as it
    // connected): its certificate for the id is a refresh of its credentials, nothing moves, and
    // its process being connected doesn't matter (docs/machines.md, "One record a machine").
    if hub.machine_of(&from) == to {
        crate::identities::record(&hub.store, "machine", &to, &cert).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
        hub.net.command(crate::net::Out::Unrevoke { name: to.clone() }).await;
        hub.store.audit("kiln".into(), "agent.certificate".into(), format!("{} has its certificate for its machine id {to}", hub.shown(&to))).await;
        return Ok(srv::json(
            StatusCode::OK,
            &crate::agent::EnrollResponse { cert_pem: cert, ca_pem: hub.pki.ca_pem.clone(), quic: hub.cfg.server.quic_advertise.clone(), name: to.clone(), label: Some(hub.shown(&to)) },
        ));
    }
    let agents = hub.agent_list().await;
    let Some(me) = agents.iter().find(|a| a.name == from) else {
        return Err(err(StatusCode::FORBIDDEN, format!("{from} is not enrolled here any more; enroll it again")));
    };
    if let Some(said) = hub.store.agent_machines().await.ok().and_then(|m| m.get(&from).cloned()).filter(|s| *s != to) {
        return Err(err(StatusCode::FORBIDDEN, format!("{from} said it is machine {said}, not {to}")));
    }
    // The id known already and connected: a copy of that machine, or the machine itself (docs/machines.md).
    if let Some(known) = agents.iter().find(|a| a.name == to && a.connected) {
        return Err(err(StatusCode::CONFLICT, format!("a runner is known by {to} already and is connected ({})", known.shown())));
    }
    // Another record of the machine still connected: what it holds would move from under it.
    let dups = hub.duplicates_of(&to).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    if let Some((dup, _)) = dups.iter().find(|(n, up)| *up && *n != from) {
        return Err(err(StatusCode::CONFLICT, format!("{} says it is machine {to} too and is connected: this runner moves once it isn't", hub.shown(dup))));
    }
    // Its certificate for the id, recorded first as every one kiln issues is (W27-R01): forgetting
    // the runner then revokes its key, its certificate for its name with it.
    crate::identities::record(&hub.store, "machine", &to, &cert).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    // Its name, as its label, unless another machine shows by it.
    let label = free_agent_name(hub, &from, &me.hostname, Some(&to)).await?;
    // The id's own label, when it's known already and has one.
    let label = match hub.move_runner(&from, &to, &label, &me.hostname).await {
        Ok(kept) => kept,
        Err(e) => return Err(err(StatusCode::CONFLICT, format!("{e:#}"))),
    };
    // The id may connect, as a machine enrolled under it may: a revoked key stays refused.
    hub.net.command(crate::net::Out::Unrevoke { name: to.clone() }).await;
    hub.store.audit("kiln".into(), "agent.moved".into(), format!("{from} is known by its machine id {to}, shown as {label}")).await;
    tracing::info!(from = %from, to = %to, label = %label, "a runner moved to its machine id");
    // The machine's other records, not connected, fold in too.
    if let Err(e) = hub.fold_duplicates(&to).await {
        tracing::warn!(machine = %to, error = %format!("{e:#}"), "couldn't fold this machine's other records");
    }
    Ok(srv::json(
        StatusCode::OK,
        &crate::agent::EnrollResponse { cert_pem: cert, ca_pem: hub.pki.ca_pem.clone(), quic: hub.cfg.server.quic_advertise.clone(), name: to, label: Some(label) },
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

/// The targets the release workflow builds. Macs are Apple silicon only: kiln doesn't run on Intel
/// Macs.
pub const RELEASE_TARGETS: [&str; 5] = [
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
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
            h.emit(&crate::wire::events::ServerEvent::Discovery);
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
    hub.emit(&crate::wire::events::ServerEvent::Discovery);
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
    hub.emit(&crate::wire::events::ServerEvent::Discovery);
    Ok(srv::json(StatusCode::OK, &json!({ "invited": true })))
}

/// `DELETE /api/discovery/invite/:instance`.
async fn discovery_uninvite(hub: &Hub, id: &str) -> Response {
    if hub.discovery.invites.revoke(id) {
        hub.store.audit("admin".into(), "discovery.uninvite".into(), id.to_owned()).await;
        hub.emit(&crate::wire::events::ServerEvent::Discovery);
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
    hub.emit(&crate::wire::events::ServerEvent::JoinRequests);
    // The machine's entry under discovery now has a pending request.
    hub.emit(&crate::wire::events::ServerEvent::Discovery);
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
                hub.emit(&crate::wire::events::ServerEvent::JoinRequests);
                hub.emit(&crate::wire::events::ServerEvent::Discovery);
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

/// Whether a machine asking for `wanted` from `hostname` is the server's own machine (`server_host`)
/// asking for its local agent's name (`local`): that agent is already the machine's runner.
fn own_machines_runner(local: Option<&str>, server_host: &str, wanted: &str, hostname: &str) -> bool {
    local == Some(wanted) && hostname.eq_ignore_ascii_case(server_host)
}

/// A name for an approved machine: the one it asked for, unless another machine's agent (or the
/// server's own local agent) has it, then `<name>-2`, `<name>-3`, … Another machine is one with
/// another machine id ([`crate::join::free_name`]). The server's own machine asking for its local
/// agent's name, or with its machine id, is refused, never named `<name>-2`: a machine runs one kiln
/// runner.
async fn free_agent_name(hub: &Hub, wanted: &str, hostname: &str, machine_id: Option<&str>) -> Result<String, ApiError> {
    let agents = hub.agent_list().await;
    let machines = hub.store.agent_machines().await.unwrap_or_default();
    let local = hub.cfg.local_agent.enabled.then(|| crate::pki::sanitize_name(&hub.cfg.local_agent_name()));
    // The server's own machine: by its local agent's name and host name, or by its machine id
    // (docs/machines.md), whatever name it asks for.
    let own = own_machines_runner(local.as_deref(), &crate::config::Config::hostname(), wanted, hostname)
        || (hub.cfg.local_agent.enabled && crate::join::machine_key(machine_id).is_some_and(|k| k == hub.cfg.local_agent_key()));
    if own {
        let runner = local.as_deref().unwrap_or(wanted);
        return Err(err(
            StatusCode::CONFLICT,
            format!("{hostname} already runs this server's own runner, {runner}, and a machine runs one kiln runner: turn the server's off (`[local_agent] enabled = false`) to run a separate one here"),
        ));
    }
    // Each agent holds its name and, one known by its machine id, its label too; its machine is
    // the one its runner said, else the id it's known by.
    let holders: Vec<crate::join::Holder<'_>> = agents
        .iter()
        .flat_map(|a| {
            let machine_id = machines.get(&a.name).map(String::as_str).or_else(|| crate::join::is_machine_key(&a.name).then_some(a.name.as_str()));
            let named = crate::join::Holder { name: &a.name, hostname: &a.hostname, machine_id };
            let labeled = a.label.as_deref().map(|l| crate::join::Holder { name: l, hostname: &a.hostname, machine_id });
            std::iter::once(named).chain(labeled)
        })
        .collect();
    Ok(crate::join::free_name(wanted, hostname, machine_id, local.as_deref(), &holders))
}

/// What a machine that's approved or enrolls is known by, and shown as (docs/machines.md): its
/// label is the name it asked for unless another machine has it ([`free_agent_name`]); its name,
/// the key every part of kiln knows it by, is its machine id when it sent one, else that label.
async fn name_and_label(hub: &Hub, wanted: &str, hostname: &str, machine_id: Option<&str>) -> Result<(String, String), ApiError> {
    let label = free_agent_name(hub, wanted, hostname, machine_id).await?;
    let name = crate::join::machine_key(machine_id).unwrap_or_else(|| label.clone());
    Ok((name, label))
}

/// Why a machine asking to be known by `name`, a machine id, isn't: a machine known by that id is
/// connected now, so this one is a copy of it (a VM cloned with its `/etc/machine-id`), and the
/// two would be one runner (docs/machines.md). The same machine joining again has its runner
/// stopped first.
fn a_copy_of_a_connected_machine(hub: &Hub, name: &str) -> Option<String> {
    let connected = crate::join::is_machine_key(name) && hub.agents.load().get(name).is_some_and(|a| a.connected);
    connected.then(|| {
        format!(
            "a machine known by this machine's id ({name}) is connected now: if this one is a copy of it, give it an id of its own \
             (`systemd-machine-id-setup` on Linux, sysprep on Windows) and ask again"
        )
    })
}

/// How the audit and the logs say which agent: its label and its id, else its name.
fn shown(name: &str, label: Option<&str>) -> String {
    label.map_or_else(|| name.to_owned(), |l| format!("{l} ({name})"))
}

/// `POST /api/join-requests/:id/approve`: sign the machine's CSR exactly as enrollment does.
async fn approve_join(hub: &Hub, id: &str, req: Request, by: &str) -> R {
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
    let (name, label) = name_and_label(hub, &wanted, &c.hostname, c.machine_id.as_deref()).await?;
    if let Some(why) = a_copy_of_a_connected_machine(hub, &name) {
        return Err(err(StatusCode::CONFLICT, why));
    }
    let label = (label != name).then_some(label);
    // An outpost that asked instead of bringing an invite gets what an invited one gets.
    let outpost = c.labels.iter().any(|l| l == crate::proto::OUTPOST_LABEL);
    let labels = decision.labels.as_deref().map(crate::join::clean_labels).filter(|l| !l.is_empty()).unwrap_or(c.labels);
    let days = if outpost { crate::pki::OUTPOST_DAYS } else { 365 };
    let cert = hub.pki.sign_csr(&c.csr, &name, days).map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    crate::identities::record(&hub.store, "machine", &name, &cert).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    crate::identities::enrolled(&hub.store, &name, by).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let quic = hub.cfg.server.quic_advertise.clone();
    let quic_port = hub.discovery.advert.lock().quic;
    // Known by its machine id: the name people see is kept with it before it can connect.
    if let Some(l) = &label
        && let Err(e) = hub.set_label(&name, l, &c.hostname).await
    {
        tracing::warn!(agent = %name, label = %l, error = %format!("{e:#}"), "couldn't keep the approved machine's label");
    }
    let approval = crate::join::Approval {
        name: name.clone(),
        labels: labels.clone(),
        cert_pem: cert,
        ca_pem: hub.pki.ca_pem.clone(),
        quic,
        quic_port,
        label: label.clone(),
    };
    if let Err(r) = hub.joins.approve(id, approval, Instant::now()) {
        return Ok(refused(&r));
    }
    // A name forgotten earlier in this server's life is welcome again.
    hub.net.command(crate::net::Out::Unrevoke { name: name.clone() }).await;
    let what = if outpost { "outpost.join.approve" } else { "agent.join.approve" };
    hub.store.audit("admin".into(), what.into(), format!("{} ({})", shown(&name, label.as_deref()), c.hostname)).await;
    tracing::info!(agent = %name, label = label.as_deref().unwrap_or(""), host = %c.hostname, outpost, "join request approved");
    if let Some(instance) = asked {
        hub.discovery.invites.revoke(&instance);
    }
    hub.emit(&crate::wire::events::ServerEvent::JoinRequests);
    hub.emit(&crate::wire::events::ServerEvent::Discovery);
    Ok(srv::json(StatusCode::OK, &json!({ "id": id, "status": "approved", "name": name, "label": label, "labels": labels })))
}

/// `POST /api/join-requests/:id/deny`.
async fn deny_join(hub: &Hub, id: &str) -> R {
    match hub.joins.deny(id, Instant::now()) {
        Ok(name) => {
            hub.store.audit("admin".into(), "agent.join.deny".into(), name.clone()).await;
            tracing::info!(agent = %name, "join request denied");
            hub.emit(&crate::wire::events::ServerEvent::JoinRequests);
            hub.emit(&crate::wire::events::ServerEvent::Discovery);
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
/// or fetched here (System → Builds), otherwise a redirect to the release build of the same
/// version on GitHub.
async fn download_for(hub: &Hub, target: &str) -> R {
    if target == TARGET {
        return download_binary().await;
    }
    if let Some((b, body)) = crate::updates::stream(hub, target) {
        let r = srv::response(StatusCode::OK, "application/octet-stream", srv::channel_body(body));
        return Ok(srv::with_header(r, header::CONTENT_LENGTH, &b.size.to_string()));
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
    let (kept, body) = crate::updates::stream(hub, target).filter(|(m, _)| m.build == build).ok_or_else(|| err(StatusCode::NOT_FOUND, format!("kiln has no build {build} for {target}")))?;
    let r = srv::response(StatusCode::OK, "application/octet-stream", srv::channel_body(body));
    Ok(srv::with_header(r, header::CONTENT_LENGTH, &kept.size.to_string()))
}

fn build_token_ok(hub: &Hub, headers: &HeaderMap, job: &str) -> bool {
    bearer(headers).is_some_and(|t| crate::util::ct_eq(t.as_bytes(), crate::fleet::token(hub, job).as_bytes()))
}

/// `GET /api/updates/jobs/{id}/source`: the source of the commit a machine builds, for itself or
/// for another machine's platform (`crate::fleet`), as tar.
async fn build_source(hub: &Hub, id: &str, headers: &HeaderMap) -> R {
    if !build_token_ok(hub, headers, id) {
        return Err(err(StatusCode::UNAUTHORIZED, "bad build token"));
    }
    let stdout = crate::fleet::source(hub, id).await.map_err(|e| err(StatusCode::NOT_FOUND, format!("{e:#}")))?;
    Ok(srv::response(StatusCode::OK, "application/x-tar", srv::channel_body(stdout)))
}

/// `POST /api/updates/jobs/{id}/binary?sha256=<hex>`: what a machine built; the binary as the body
/// when it is kept for its platform, else nothing.
async fn build_binary(hub: &Arc<Hub>, id: &str, q: &HashMap<String, String>, req: Request) -> R {
    if !build_token_ok(hub, req.headers(), id) {
        return Err(err(StatusCode::UNAUTHORIZED, "bad build token"));
    }
    let sha256 = q.get("sha256").cloned().unwrap_or_default();
    let arrived = arriving_binary(hub, req).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    crate::fleet::on_built(hub, id, &sha256, Some(arrived)).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

/// A binary sent as a request's body, written to a file as it comes and hashed as it passes
/// (`crate::updates::Arriving`): never held whole in memory, so none is too large but for the disk.
async fn arriving_binary(hub: &Hub, req: Request) -> anyhow::Result<crate::updates::Arrived> {
    let mut body = req.into_body();
    let mut arriving = crate::updates::Arriving::new(&crate::updates::arriving_dir(hub))?;
    while let Some(chunk) = srv::next_chunk(&mut body).await? {
        arriving.write(&chunk)?;
    }
    arriving.done()
}

// ------------------------------------------------------------------ updates (System → Updates)

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
            "waits": {
                "now": crate::updates::waits(hub).await,
                "config": crate::updates::Waits::of(&hub.cfg.updates),
                "changed": crate::updates::waits_changed(hub).await,
            },
            "skew": skew_view(hub).await,
        }),
    )
}

/// `GET /api/updates/skew`: the version-skew tolerances (`crate::skew`), kiln.toml's beside
/// them, the commits each takes (the server's first), and where each connected runner stands.
async fn skew_view(hub: &Hub) -> serde_json::Value {
    let now = crate::skew::tolerance(hub).await;
    let runners: Vec<serde_json::Value> =
        hub.agents.load().values().filter(|a| a.connected && !a.ephemeral).map(|a| json!({ "name": a.name, "skew": a.skew })).collect();
    json!({
        "now": now,
        "config": crate::skew::Tolerance::of(&hub.cfg.updates),
        "changed": crate::skew::changed(hub).await,
        "most": crate::skew::MOST,
        "server": COMMIT,
        "commits": crate::skew::window(hub, now).await,
        "runners": runners,
    })
}

/// `PUT /api/updates/skew` `{"full": 100, "upgrade": 1000}`: how many commits behind the server's a
/// runner may be and do everything, or connect to be upgraded; every connected runner is weighed
/// again (kiln.toml's `[updates]` says until then).
async fn set_skew(hub: &Arc<Hub>, req: Request) -> R {
    let t: crate::skew::Tolerance = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    t.check().map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    crate::skew::set(hub, Some(t)).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    hub.store.audit("admin".into(), "upgrade.skew".into(), format!("full within {} commits, upgrade within {}", t.full, t.upgrade)).await;
    hub.emit(&crate::wire::events::ServerEvent::Updates);
    Ok(srv::status(StatusCode::NO_CONTENT))
}

/// `GET /api/updates/jobs/{id}`: the upgrade, and, a runner's blue/green one, its replant's record
/// of what it handed on (docs/ci.md W27-R08; the Transfers plan's step 8): its machine's runner role
/// and its agent sessions, each a transfer signed by both sides (`transfers`), and what moved
/// without one (`moved_without_a_transfer`).
async fn update_job(hub: &Arc<Hub>, j: crate::updates::Job) -> R {
    let mut v = serde_json::to_value(&j).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if let Some((machine, since, sessions)) = crate::updates::Updates::replant(&j) {
        let r = crate::custody::replanted(&hub.store, &hub.pki.ca_der, &hub.pki.organization(), &machine, since, &sessions).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
        if let (Some(o), Some(r)) = (v.as_object_mut(), r.as_object()) {
            o.extend(r.clone());
        }
    }
    Ok(srv::json(StatusCode::OK, &v))
}

/// `DELETE /api/updates/skew`: back to what kiln.toml says.
async fn reset_skew(hub: &Arc<Hub>) -> R {
    crate::skew::set(hub, None).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    hub.store.audit("admin".into(), "upgrade.skew".into(), "as kiln.toml says".to_owned()).await;
    hub.emit(&crate::wire::events::ServerEvent::Updates);
    Ok(srv::status(StatusCode::NO_CONTENT))
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
#[derive(Deserialize, Default)]
struct RunnerUpgrade {
    /// "download" or "build"; by default whichever kiln can.
    #[serde(default)]
    way: Option<crate::updates::Way>,
}

/// `POST /api/updates/runners/{name}`, optionally `{"way": "download" | "build"}`: upgrade that
/// machine's runner to the newest kiln now, that way (a retry too).
async fn upgrade_runner(hub: &Arc<Hub>, name: &str, req: Request) -> R {
    let bytes = srv::body_bytes(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let b: RunnerUpgrade = if bytes.iter().all(u8::is_ascii_whitespace) {
        RunnerUpgrade::default()
    } else {
        serde_json::from_slice(&bytes).map_err(|e| err(StatusCode::BAD_REQUEST, format!("the body isn't an upgrade: {e}")))?
    };
    match crate::updates::upgrade_runner(hub, name, b.way).await {
        Ok(job) => Ok(srv::json(StatusCode::ACCEPTED, &job)),
        Err(e) => Err(err(StatusCode::CONFLICT, format!("{e:#}"))),
    }
}

/// `PUT` (the body's rules) or `DELETE` (none: the scope above's apply) the rules for the copies of
/// runners' trees other runners keep, `whose` (docs/reach.md §5.1). Runners have them within
/// 20 seconds.
async fn keep_rules(hub: &Arc<Hub>, caller: &crate::authz::Caller, whose: crate::reach::copies::Whose<'_>, put: bool, req: Request) -> R {
    let rules = if put {
        let bytes = srv::body_bytes(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
        Some(crate::reach::copies::rules_of(&bytes).map_err(|e| err(StatusCode::BAD_REQUEST, e))?)
    } else {
        None
    };
    let said = rules.as_ref().filter(|r| !r.is_empty()).map_or_else(|| "as the scope above says".to_owned(), |r| serde_json::to_string(r).unwrap_or_default());
    hub.copies.set(hub, whose, rules).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    hub.store.audit(caller.name.clone(), "reach.keep".into(), format!("{}: {said}", whose.said())).await;
    hub.emit(&crate::wire::events::ServerEvent::Keep);
    Ok(srv::status(StatusCode::NO_CONTENT))
}

/// `POST /api/reach/keep/now`, `POST /api/reach/keep/runners/{name}/now`: that runner (or every
/// one) lets go now of what its rules don't keep, within 20 seconds.
async fn keep_now(hub: &Arc<Hub>, caller: &crate::authz::Caller, runner: Option<&str>) -> R {
    let asked = hub.copies.ask(hub, runner);
    if asked.is_empty() {
        return Err(err(StatusCode::NOT_FOUND, runner.map_or_else(|| "no runner to ask".to_owned(), |r| format!("no runner {r}"))));
    }
    hub.store.audit(caller.name.clone(), "reach.keep.now".into(), format!("let go now of what the rules don't keep on {}", asked.join(", "))).await;
    hub.emit(&crate::wire::events::ServerEvent::Keep);
    Ok(srv::json(StatusCode::ACCEPTED, &json!({ "asked": asked })))
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
    hub.emit(&crate::wire::events::ServerEvent::Updates);
    // Its gauges at once, and an automatic one brought up to date.
    hub.updates.fleet.wake();
    Ok(srv::status(StatusCode::NO_CONTENT))
}

/// `PUT /api/updates/waits` `{"upgrade_minutes": 15, "wait_for_jobs_minutes": 360, "build_minutes": 60}`:
/// how long kiln waits on runners' upgrades from now on, those under way too (kiln.toml's
/// `[updates]` says until then).
async fn set_waits(hub: &Arc<Hub>, req: Request) -> R {
    let w: crate::updates::Waits = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    w.check().map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    crate::updates::set_waits(hub, Some(w)).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let said = format!("upgrades {}, waiting for jobs {}, builds {}", w.upgrade_minutes, w.wait_for_jobs_minutes, w.build_minutes);
    hub.store.audit("admin".into(), "upgrade.waits".into(), format!("{said} (minutes)")).await;
    hub.emit(&crate::wire::events::ServerEvent::Updates);
    Ok(srv::status(StatusCode::NO_CONTENT))
}

/// `DELETE /api/updates/waits`: back to what kiln.toml says.
async fn reset_waits(hub: &Arc<Hub>) -> R {
    crate::updates::set_waits(hub, None).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    hub.store.audit("admin".into(), "upgrade.waits".into(), "as kiln.toml says".to_owned()).await;
    hub.emit(&crate::wire::events::ServerEvent::Updates);
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
    let arrived = arriving_binary(hub, req).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let b = crate::updates::store(hub, &target, &arrived, "upload").await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    hub.store.audit("admin".into(), "upgrade.binary".into(), format!("uploaded build {} for {target}", b.build)).await;
    hub.emit(&crate::wire::events::ServerEvent::Updates);
    Ok(srv::json(StatusCode::OK, &b))
}

/// `DELETE /api/updates/binaries/{target}`.
async fn delete_binary(hub: &Hub, target: &str) -> R {
    if !crate::updates::unstore(hub, target).await {
        return Err(err(StatusCode::NOT_FOUND, format!("no binary is kept for {target}")));
    }
    hub.store.audit("admin".into(), "upgrade.binary".into(), format!("removed the binary for {target}")).await;
    hub.emit(&crate::wire::events::ServerEvent::Updates);
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
    // What GitHub said, as events and metrics (`crate::observe::github`).
    crate::observe::github::webhook(&event, &body);
    // Without the CI engine a webhook only hints; the poll decides what changed (GitOps: state
    // comes from git). With it, GitHub's payload reaches the workflows (docs/ci.md W03-R07).
    let hub = Arc::clone(hub);
    crate::rt::spawn(async move {
        if let Err(e) = crate::ci::hooks::github(&hub, &event, delivery, &body).await {
            tracing::warn!(event = %event, error = %format!("{e:#}"), "webhook");
            crate::ci::hooks::want_github(&hub, &body).await;
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
    let h = Arc::clone(hub);
    let note = move |u| {
        let h = Arc::clone(&h);
        crate::rt::spawn(async move {
            let _ = h.note_unread(u, None).await;
        });
    };
    crate::tunnel::serve(&hub.tunnels, crate::tunnel::listener_for(bind), req, note).map_err(|(status, why)| err(status, why))
}

// ------------------------------------------------------------------ events ws

/// What a caller of some repositories hears of the event stream (W10): events about those
/// repositories (a run's, a job's run's, any that names one) and their own inbox's; the rest are
/// the server's.
struct Narrow {
    repos: std::collections::HashSet<String>,
    person: String,
    /// The repository of each run a job event was about, as looked up.
    runs: HashMap<String, Option<String>>,
}

/// The agents an events socket's caller sees (docs/agents.md, "Whose agents"): an agent's events
/// (`harness`, `harness_closed`, `agent_change`) reach it only for those; every other event passes.
struct AgentsSeen {
    caller: crate::authz::Caller,
    /// Each session's answer, as asked (whom a session acts for doesn't change).
    seen: HashMap<String, bool>,
}

impl AgentsSeen {
    /// Read from what the feed read of the frame once for every socket (`feed::Meta`), not from
    /// its text again.
    async fn lets(&mut self, hub: &Hub, m: &crate::wire::events::feed::Meta) -> bool {
        use crate::wire::events::feed::Agent;
        let id = match &m.agent {
            Agent::Free => return true,
            Agent::Of(None) => return false,
            Agent::Of(Some(id)) => id,
        };
        if let Some(&ok) = self.seen.get(id) {
            return ok;
        }
        let ok = crate::harness::lineage::access(hub, &self.caller, id).await >= crate::harness::lineage::Access::See;
        if self.seen.len() > 10_000 {
            self.seen.clear();
        }
        self.seen.insert(id.to_owned(), ok);
        ok
    }
}

impl Narrow {
    async fn lets(&mut self, hub: &Hub, m: &crate::wire::events::feed::Meta) -> bool {
        if !m.parsed {
            return false;
        }
        match m.kind.as_str() {
            "resync" => return true,
            "inbox" => return m.person.as_deref() == Some(self.person.as_str()),
            _ => {}
        }
        if let Some(r) = &m.repo {
            return self.repos.contains(r);
        }
        let Some(run) = &m.job_run else { return false };
        if !self.runs.contains_key(run) {
            if self.runs.len() > 10_000 {
                self.runs.clear();
            }
            let repo = hub.store.run(run.clone()).await.ok().flatten().map(|r| r.repo_name);
            self.runs.insert(run.clone(), repo);
        }
        self.runs.get(run).cloned().flatten().is_some_and(|r| self.repos.contains(&r))
    }
}

/// How an events socket's page asked to be sent (docs/wire.md, "The events socket"): batched
/// (`?batch=1`, or `?after=`), only some types (`?topics=a,b`), from where it was (`?after=<at>`).
/// A page that asks for none of it is sent every frame as it comes, as before.
struct Asked {
    batch: bool,
    topics: Option<std::collections::HashSet<String>>,
    after: Option<String>,
}

impl Asked {
    fn of(q: &HashMap<String, String>) -> Asked {
        let after = q.get("after").filter(|a| !a.is_empty()).cloned();
        Asked { batch: after.is_some() || q.get("batch").is_some_and(|b| b == "1"), topics: q.get("topics").map(|t| crate::wire::events::feed::topics(t)), after }
    }
}

fn events_ws(hub: Arc<Hub>, req: Request, asked: Asked, narrow: Option<Narrow>, agents: Option<AgentsSeen>) -> R {
    let sock = ui::Socket::of(Wire::EventsUp, Wire::EventsDown, &caller_name(&req), None);
    let (resp, upgrade) = ws::accept_commands(req).map_err(|r| ApiError(r.status, r.reason.into()))?;
    crate::rt::spawn(async move {
        let Ok(socket) = upgrade.await else { return };
        let m = crate::metrics::global();
        m.ws_clients.add(1);
        if let Err(e) = pump_events(&hub, socket, &sock, asked, narrow, agents).await {
            tracing::debug!(error = %e, "events stream ended");
        }
        m.ws_clients.add(-1);
    });
    Ok(resp)
}

/// Who a request is, as the audit log names them (`admin`, `user:<name>` ...).
fn caller_name(req: &Request) -> String {
    req.extensions().get::<crate::authz::Caller>().map_or_else(|| "admin".to_owned(), |c| c.name.clone())
}

/// Whether frame `f` goes to this socket: a type it asked for, about what its caller may see.
async fn events_let(hub: &Hub, out: &crate::wire::events::feed::Outbox, narrow: &mut Option<Narrow>, agents: &mut Option<AgentsSeen>, f: &crate::wire::events::feed::Frame) -> bool {
    let m = f.meta();
    if !out.wants(&m.kind) {
        return false;
    }
    (match narrow.as_mut() {
        Some(n) => n.lets(hub, m).await,
        None => true,
    }) && match agents.as_mut() {
        Some(a) => a.lets(hub, m).await,
        None => true,
    }
}

/// The events socket (docs/wire.md, "The events socket"): what the feed has after where the page
/// is, as it comes. A page that asked for batches gets them a window at a time ([`feed::WINDOW`]),
/// coalesced, each saying where the feed is; one that didn't gets each frame as it comes. Either
/// that falls further behind than the feed keeps is told `resync`.
///
/// [`feed::WINDOW`]: crate::wire::events::feed::WINDOW
async fn pump_events(hub: &Hub, socket: ws::WebSocket, sock: &ui::Socket, asked: Asked, mut narrow: Option<Narrow>, mut agents: Option<AgentsSeen>) -> anyhow::Result<()> {
    use crate::wire::events::feed::{self, At, Since};
    use futures::FutureExt as _;
    let (mut tx, rx_ws) = socket.split();
    // The page's own messages are asked for only once its socket woke (`ws::Gated`; goal 9, plan 01a1a87b2cec
    // step 5): every frame the feed takes wakes this task, and each ask zero-filled the socket's read buffer and
    // read nothing.
    let mut rx_ws = ws::Gated::new(rx_ws);
    // The hub's events and agent sessions' own, both numbered in the feed (W21-R15), which no push or read
    // takes a lock of (`crate::wire::events::feed`; goal 4, plan 01a1a59c2a9c step 5, and goal 5's rule): no
    // lock between this socket's shard and their senders'.
    let fd = &hub.feed;
    // Listened to before the first look, for as long as the socket's open: a frame pushed while it looks
    // wakes its next wait.
    let woke = fd.listen();
    let batch = asked.batch;
    let (mut cursor, missed) = if batch { fd.resume(asked.after.as_deref()) } else { (fd.head(), false) };
    let mut out = feed::Outbox::new(asked.topics);
    if missed {
        out.resync();
    }
    // Lets the filters pass a frame without reading it when there are none.
    let everything = out.wants("") && narrow.is_none() && agents.is_none() && !batch;
    hub.emit_queue();
    // The first batch goes at once, saying where the feed is.
    let mut first = batch;
    let mut sent_at: Option<Instant> = None;
    let mut due: Option<Instant> = None;
    loop {
        cursor = take_since(hub, &mut tx, cursor, &mut out, batch, everything, &mut narrow, &mut agents).await?;
        if batch && (first || !out.is_empty()) {
            let now = Instant::now();
            let when = if first || out.len() >= feed::MAX_BATCH { now } else { due.unwrap_or_else(|| sent_at.map_or(now, |s| (s + feed::WINDOW).max(now))) };
            if when <= now {
                if let Some(text) = out.flush(At { epoch: fd.epoch(), seq: cursor }, first) {
                    tx.send(ws::text(text)).await?;
                }
                first = false;
                sent_at = Some(now);
                due = None;
                continue;
            }
            due = Some(when);
        }
        let window = async move {
            match due {
                Some(d) => crate::rt::sleep_until(d).await,
                None => futures::future::pending::<()>().await,
            }
        };
        futures::select_biased! {
            () = woke.woken().fuse() => {}
            _ = window.fuse() => {}
            msg = rx_ws.next().fuse() => {
                let came = ui::came(msg);
                if let Came::Text(t) = &came
                    && let Some(up) = events_topics(t.as_str())
                {
                    match up {
                        Ok(crate::wire::events::EventsUp::Topics { topics, since }) => {
                            let added = out.set_topics(topics.map(|t| t.into_iter().collect()));
                            // Of the types new to it, what came since the page last heard: what it
                            // loaded as it asked for them misses none of their changes.
                            if let Some(s) = since.as_deref().and_then(At::parse).filter(|s| s.epoch == fd.epoch() && !added.is_empty()) {
                                let kept = match fd.since(s.seq) {
                                    Since::Frames(f) => f,
                                    Since::Gap => fd.kept(),
                                };
                                let mut earlier = Vec::new();
                                for f in kept {
                                    if f.seq <= cursor && added.contains(&f.meta().kind) && events_let(hub, &out, &mut narrow, &mut agents, &f).await {
                                        earlier.push(f);
                                    }
                                }
                                out.add_earlier(earlier);
                            }
                        }
                        Err(r) => {
                            hub.note_refused(sock, &r).await;
                            tx.send(ws::text(crate::wire::events::ServerEvent::Refused(r).text())).await?;
                        }
                    }
                } else if hub.unread_only(sock, &mut tx, came, |r| crate::wire::events::ServerEvent::Refused(r).text()).await? {
                    return Ok(());
                }
            }
        }
    }
}

/// What came on the feed after `cursor` for an events socket (W21-R15), and where it then is: to a
/// batched one, into its outbox; to one that isn't, each frame its filters let through, as the feed's
/// own text (one allocation however many sockets send it), and `Resync` when some are gone, written as
/// one: fed, then flushed once (goal 4, plan 01a1a59c2a9c step 5; as goal 9 step 5 found, a flush an
/// event was a write to the connection an event).
#[allow(clippy::too_many_arguments)]
async fn take_since<S>(
    hub: &Hub,
    tx: &mut S,
    cursor: u64,
    out: &mut crate::wire::events::feed::Outbox,
    batch: bool,
    everything: bool,
    narrow: &mut Option<Narrow>,
    agents: &mut Option<AgentsSeen>,
) -> anyhow::Result<u64>
where
    S: futures::Sink<ws::Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    use crate::wire::events::feed::Since;
    use futures::SinkExt as _;
    let fd = &hub.feed;
    if fd.head() <= cursor {
        return Ok(cursor);
    }
    let (mut at, mut wrote) = (cursor, false);
    match fd.since(cursor) {
        Since::Gap => {
            at = fd.head();
            if batch {
                out.resync();
            } else {
                tx.feed(ws::text(crate::wire::events::ServerEvent::Resync.text())).await?;
                wrote = true;
            }
        }
        Since::Frames(frames) => {
            for f in frames {
                at = f.seq;
                if !everything && !events_let(hub, out, narrow, agents, &f).await {
                    continue;
                }
                if batch {
                    out.add(f);
                } else {
                    tx.feed(ws::shared_text(&f.text)).await?;
                    wrote = true;
                }
            }
        }
    }
    if wrote {
        tx.flush().await?;
    }
    Ok(at)
}

/// `text`, when it's a `topics` frame (`crate::wire::events::EventsUp`), read; or why it
/// couldn't be. Anything else is every socket's (`unread`).
fn events_topics(text: &str) -> Option<Result<crate::wire::events::EventsUp, ui::Refused>> {
    let v = serde_json::from_str::<serde_json::Value>(text).ok()?;
    (v.get("t").and_then(serde_json::Value::as_str) == Some("topics")).then(|| serde_json::from_value(v).map_err(|e| ui::Refused::malformed("topics", e.to_string())))
}

// ------------------------------------------------------------------ terminal ws

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
    // Who opens it: `kiln spawn --on` and `kiln agents` in it act for them.
    let opener = req.extensions().get::<crate::authz::Caller>().and_then(crate::authz::Caller::opener);
    let t: NewTerminal = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    if hub.terminals.live() >= hub.cfg.terminal.max_sessions {
        return Err(err(StatusCode::TOO_MANY_REQUESTS, "too many terminal sessions"));
    }
    let (cols, rows) = (t.cols.unwrap_or(120), t.rows.unwrap_or(40));
    let session = match t.target.strip_prefix("agent:") {
        Some(machine) => {
            if !hub.agents.load().get(machine).is_some_and(|a| a.connected) {
                return Err(err(StatusCode::CONFLICT, format!("{} is not connected", hub.shown(machine))));
            }
            // Its kiln is to be upgraded: no new terminal (`crate::skew`).
            if let Some(why) = hub.skew_holds(machine) {
                return Err(err(StatusCode::CONFLICT, why));
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
    if let Some(o) = opener {
        let _ = hub.store.kv_set(crate::mcp::terminal_opener_key(&session.id), o).await;
    }
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
    let target = q.get("target").cloned().unwrap_or_else(|| "local".into());
    // What it can't read shows on the page of the runner it's on.
    let on = existing.as_deref().and_then(|s| hub.terminals.get(s)).map_or_else(|| target.clone(), |t| t.target.clone());
    let sock = ui::Socket::of(Wire::TerminalUp, Wire::TerminalDown, &caller_name(&req), terminal_page(&on));
    let (resp, upgrade) = ws::accept_commands(req).map_err(|r| ApiError(r.status, r.reason.into()))?;
    let cols = q.get("cols").and_then(|v| v.parse().ok()).unwrap_or(120);
    let rows = q.get("rows").and_then(|v| v.parse().ok()).unwrap_or(40);
    let cmd = q.get("cmd").cloned();
    crate::rt::spawn(async move {
        let Ok(socket) = upgrade.await else { return };
        if let Err(e) = terminal_attach(hub, existing, target, (cols, rows), cmd, "admin".into(), socket, sock).await {
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
        return Err(err(StatusCode::FORBIDDEN, format!("{} may not open a shell on runner {}: a host leg's terminal needs runners.terminal there", caller.name, hub.shown(&agent))));
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
    let (resp, upgrade) = ws::accept_commands(req).map_err(|r| ApiError(r.status, r.reason.into()))?;
    let cols = q.get("cols").and_then(|v| v.parse().ok()).unwrap_or(120);
    let rows = q.get("rows").and_then(|v| v.parse().ok()).unwrap_or(40);
    let sock = ui::Socket::of(Wire::TerminalUp, Wire::TerminalDown, &caller.name, Some(Page::Runner(agent.clone())));
    let (target, actor) = (format!("leg:{id}@{agent}"), caller.name.clone());
    crate::rt::spawn(async move {
        let Ok(socket) = upgrade.await else { return };
        if let Err(e) = terminal_attach(hub, existing, target, (cols, rows), None, actor, socket, sock).await {
            tracing::debug!(error = %format!("{e:#}"), "leg terminal websocket ended");
        }
    });
    Ok(resp)
}

/// The page a terminal on `target` shows what it can't read on: its runner's (`agent:<name>`, a leg's
/// `leg:<job>@<name>`); none for the server's own.
fn terminal_page(target: &str) -> Option<Page> {
    let runner = target.strip_prefix("agent:").or_else(|| target.strip_prefix("leg:").and_then(|t| t.split_once('@')).map(|(_, m)| m));
    runner.map(|m| Page::Runner(m.to_owned()))
}

/// One browser on one session: identity, replay, then live output; input both ways. Closing the
/// socket only detaches — the shell keeps running for the next attach. What it sends that can't be
/// read is answered `refused` and noted (`sock`).
#[allow(clippy::too_many_arguments)]
async fn terminal_attach(hub: Arc<Hub>, existing: Option<String>, target: String, (cols, rows): (u16, u16), cmd: Option<String>, actor: String, socket: ws::WebSocket, sock: ui::Socket) -> anyhow::Result<()> {
    use crate::termsess::{Frame, Input};
    use futures::FutureExt as _;
    let (mut tx, mut rx) = socket.split();
    let session = match existing {
        Some(id) => match hub.terminals.get(&id) {
            Some(s) => s,
            None => {
                // Not (yet) known: e.g. its machine is reconnecting. The client retries.
                tx.send(ws::text(TerminalDown::Missing { session: id }.text())).await?;
                return Ok(());
            }
        },
        None => {
            hub.store.audit(actor, "terminal.open".into(), target.clone()).await;
            if let Some((job, machine)) = target.strip_prefix("leg:").and_then(|t| t.split_once('@')) {
                crate::termsess::open_leg(&hub, machine.to_owned(), job, cols, rows)
            } else if let Some(machine) = target.strip_prefix("agent:") {
                // Its kiln is to be upgraded: no new terminal (`crate::skew`).
                if let Some(why) = hub.skew_holds(machine) {
                    tx.send(ws::text(TerminalDown::Error { message: why.to_string() }.text())).await?;
                    return Ok(());
                }
                crate::termsess::open_agent(&hub, machine.to_owned(), cmd, cols, rows)
            } else {
                match crate::termsess::open_local(&hub, cmd, cols, rows) {
                    Ok(s) => s,
                    Err(e) => {
                        tx.send(ws::text(TerminalDown::Error { message: format!("{e:#}") }.text())).await?;
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
        tx.send(ws::text(TerminalDown::Session { session: session.info() }.text())).await?;
        if !attachment.replay.is_empty() {
            tx.send(ws::binary(attachment.replay)).await?;
        }
        if let Some((code, error)) = attachment.ended {
            let msg = match error {
                Some(message) => TerminalDown::Error { message },
                None => TerminalDown::Exit { code },
            };
            tx.send(ws::text(msg.text())).await.ok();
            return Ok(());
        }
        loop {
            futures::select_biased! {
                f = live.recv().fuse() => match f {
                    Ok(Frame::Data(d)) => tx.send(ws::binary(d)).await?,
                    Ok(Frame::Size { cols, rows }) => tx.send(ws::text(TerminalDown::Size { cols, rows }.text())).await?,
                    Ok(Frame::Exit { error: Some(e), .. }) if e == "__reconnect__" => {
                        tx.send(ws::text(TerminalDown::Reconnect.text())).await.ok();
                        return Ok(());
                    }
                    Ok(Frame::Exit { code, error }) => {
                        let msg = match error {
                            Some(message) => TerminalDown::Error { message },
                            None => TerminalDown::Exit { code },
                        };
                        tx.send(ws::text(msg.text())).await.ok();
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
                m = rx.next().fuse() => match ui::came(m) {
                    // Keystrokes.
                    Came::Binary(b) => session.send(Input::Data(b.to_vec())),
                    Came::Text(t) => match ui::terminal_up(t.as_str()) {
                        Ok(Up::Frame(TerminalUp::Resize { cols, rows })) => session.send(Input::Resize { cols, rows }),
                        Ok(Up::Frame(TerminalUp::Input { data })) => session.send(Input::Data(data.into_bytes())),
                        // It keeps the socket alive; nothing to do.
                        Ok(Up::Frame(TerminalUp::Ping)) => {}
                        Ok(Up::Unread(f)) => hub.note_told(&sock, &f).await,
                        Err(r) => {
                            hub.note_refused(&sock, &r).await;
                            tx.send(ws::text(TerminalDown::Refused(r).text())).await?;
                        }
                    },
                    Came::Control => {}
                    Came::Gone => return Ok(()),
                    Came::Ends { close, refused } => {
                        hub.note_refused(&sock, &refused).await;
                        let _ = tx.close(Some(close)).await;
                        return Ok(());
                    }
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
/// Where agents can have workspaces of their own (docs/agentic.md, W), by target (`local`,
/// `agent:<name>`): `{available, why_not?, mount?}` for the server and each connected runner.
async fn harness_workspaces(hub: &Hub) -> Response {
    let mut out = serde_json::Map::new();
    let mut targets = vec!["local".to_owned()];
    targets.extend(hub.agents.load().values().filter(|a| a.connected && !a.ephemeral).map(|a| format!("agent:{}", a.name)));
    for t in targets {
        let v = match crate::harness::lineage::workspaces_on(hub, &t).await {
            Ok(()) => json!({"available": true}),
            Err(why) => json!({"available": false, "why_not": why}),
        };
        out.insert(t, v);
    }
    srv::json(StatusCode::OK, &serde_json::Value::Object(out))
}

fn harness_kinds(hub: &Hub, q: &HashMap<String, String>) -> Response {
    let all = hub.harness.kinds();
    match q.get("target") {
        Some(t) => srv::json(StatusCode::OK, &all.get(t).cloned().unwrap_or_default()),
        None => srv::json(StatusCode::OK, &all),
    }
}

/// A machine's past conversations to continue (`?target=local|agent:<name>`, `kind=claude|codex|devin`,
/// `limit=`): newest first; resume one with `spec.resume = id`.
async fn harness_history(hub: &Hub, caller: &crate::authz::Caller, q: &HashMap<String, String>) -> R {
    let target = q.get("target").map_or("local", String::as_str);
    if target != "local" && target.strip_prefix("agent:").is_none_or(str::is_empty) {
        return Err(err(StatusCode::BAD_REQUEST, "target is \"local\" or \"agent:<name>\""));
    }
    may_run_on(caller, target, "read the sessions that machine keeps")?;
    let kind = q.get("kind").filter(|k| !k.is_empty()).cloned();
    if kind.as_deref().is_some_and(|k| k != "claude" && k != "codex" && k != "devin") {
        return Err(err(StatusCode::BAD_REQUEST, "kind is \"claude\", \"codex\" or \"devin\""));
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
/// JSON encoded before it was asked for (a view's, encoded once per change of it): sent as it is.
fn pre_encoded(json: bytes::Bytes) -> Response {
    srv::response(StatusCode::OK, "application/json", srv::full(json))
}

/// A read kept ahead as it answers ([`crate::reads::Made`]): its JSON as it is, with the weak ETag made with it, which
/// `crate::http::compress::finish` takes as the answer's validator, digesting nothing (goal 9, plan 01a1a87b2cec step 3).
fn pre_named(made: crate::reads::Made) -> Response {
    let mut r = pre_encoded(made.json);
    if let Some(etag) = made.etag {
        r.headers_mut().insert(header::ETAG, etag);
    }
    r
}

/// How long a join of the sessions' list takes, as the joins so far took ([`harness_sessions`]; goal 10, plan
/// 01a1a87b2dae step 5): RFC 6298's estimator of a round trip (§2), learned the same way from each join's time, a smoothed
/// mean and a mean deviation in nanoseconds (gains 1/8 and 1/4), and the bound it puts on the next, the mean and four
/// deviations (its retransmission timeout's K). No lock: two joins learned at once may lose one's sample, as an
/// estimate of a mean can.
#[derive(Debug, Default)]
struct Joins {
    /// The smoothed mean, ns: 0 before the first join (none takes nothing).
    mean_ns: std::sync::atomic::AtomicU64,
    /// The mean deviation, ns.
    dev_ns: std::sync::atomic::AtomicU64,
}

impl Joins {
    /// A join took `took`: the first sets the mean to it and the deviation to half of it (RFC 6298 §2.2), each after
    /// moves them by their gains (§2.3).
    fn took(&self, took: Duration) {
        let ns = u64::try_from(took.as_nanos()).unwrap_or(u64::MAX).max(1);
        let mean = self.mean_ns.load(std::sync::atomic::Ordering::Relaxed);
        if mean == 0 {
            self.dev_ns.store(ns / 2, std::sync::atomic::Ordering::Relaxed);
            self.mean_ns.store(ns, std::sync::atomic::Ordering::Relaxed);
            return;
        }
        let dev = self.dev_ns.load(std::sync::atomic::Ordering::Relaxed);
        self.dev_ns.store((dev - dev / 4).saturating_add(mean.abs_diff(ns) / 4), std::sync::atomic::Ordering::Relaxed);
        self.mean_ns.store((mean - mean / 8).saturating_add(ns / 8).max(1), std::sync::atomic::Ordering::Relaxed);
    }

    /// What the next join is bounded by: the mean and four deviations (RFC 6298 §2.3's K); none before the first join.
    fn bound_ns(&self) -> Option<u64> {
        let mean = self.mean_ns.load(std::sync::atomic::Ordering::Relaxed);
        (mean > 0).then(|| mean.saturating_add(self.dev_ns.load(std::sync::atomic::Ordering::Relaxed).saturating_mul(4)))
    }

    /// Whether the next join is to be made on the request's shard: the joins so far bound it within the shard's step
    /// quantum (`quantum_ns`, goal 9's: no poll past it). Not before a join was learned, nor off a shard.
    fn here(&self, quantum_ns: Option<u64>) -> bool {
        self.bound_ns().zip(quantum_ns).is_some_and(|(bound, quantum)| bound <= quantum)
    }
}

/// The sessions' list's joins so far, wherever they were made.
static JOINS: Joins = Joins { mean_ns: std::sync::atomic::AtomicU64::new(0), dev_ns: std::sync::atomic::AtomicU64::new(0) };

/// The sessions' list as [`crate::harness::registry::Registry::list_tagged`] finds it or joins it, and how long its join
/// held its thread on the CPU where it joined it (`None` where it found it as it was): what it would hold a shard for.
/// The thread's own time (`crate::shard_health::CpuClock`), which the host's holds of it don't count in: a compute
/// worker beside CPU hogs waits for its CPU a tick at a time, and joins timed so would read as longer than any quantum and
/// keep every read on the pool. Its wall time where the OS keeps no time for a thread (Windows).
fn list_timed(hub: &Hub) -> ((bytes::Bytes, Option<http::HeaderValue>), Option<Duration>) {
    let mut cpu = crate::shard_health::CpuClock::Unchosen;
    let (began, on_cpu, joins) = (Instant::now(), cpu.read(), hub.harness.list_joins());
    let listed = hub.harness.list_tagged();
    let held = on_cpu.zip(cpu.read()).map_or_else(|| began.elapsed(), |(from, to)| Duration::from_nanos(to.saturating_sub(from)));
    (listed, (hub.harness.list_joins() != joins).then_some(held))
}

/// `GET /api/harness/sessions`: the sessions `caller` sees, oldest first. For one who sees them all,
/// each one's info as its owner encoded it once, joined (goal 10, plan 01a1a87b2dae step 2); an agent
/// sees by lineage, found as it reads.
async fn harness_sessions(hub: &Arc<Hub>, caller: &crate::authz::Caller) -> Response {
    if crate::harness::lineage::sees_all(caller) {
        // With its weak ETag, named once for the reads that find the list as it was: its body isn't digested again
        // for each (goal 9, plan 01a1a87b2cec step 5; `crate::http::compress::finish`). A join reads every session's
        // view, copies each one's info into the list (38 KB in kiln-lab's realistic profile, whose eight agents at work
        // change it between nearly any two reads) and digests the infos that changed. It's made here, on the request's
        // shard, where the joins so far bound it within the shard's quantum (`Joins`; goal 9 step 3's bound on a poll),
        // else on the compute pool (goal 10, plan 01a1a87b2dae step 5): there each read waited on two wakes across
        // threads, a worker's and its own shard's, and on the pool's queue behind snapshots' and pages' compression
        // (g9p16: its jobs' wait for a thread 6.9 ms at p99 calm, 25 ms with hogs, where the API's reads were to be
        // under a millisecond at p99.9). Where the pool's job is lost (it panicked), here.
        let ((json, etag), joined) = if JOINS.here(hyper_rt::futures::step_budget_ns()) {
            list_timed(hub)
        } else {
            let h = Arc::clone(hub);
            match crate::rt::compute(move || list_timed(&h)).await {
                Ok(listed) => listed,
                Err(e) => {
                    tracing::debug!(error = %format!("{e:#}"), "the sessions' list wasn't found off the shard: found here");
                    list_timed(hub)
                }
            }
        };
        if let Some(took) = joined {
            JOINS.took(took);
        }
        let mut r = pre_encoded(json);
        if let Some(etag) = etag {
            r.headers_mut().insert(header::ETAG, etag);
        }
        return r;
    }
    srv::json(StatusCode::OK, &crate::harness::lineage::seen(hub, caller, hub.harness.list()).await)
}

async fn conversation_events(hub: &Hub, id: &str, q: &HashMap<String, String>) -> R {
    let Some(store) = hub.harness.convo.clone() else { return Err(err(StatusCode::NOT_FOUND, "this server keeps no conversations (KILN_CONVERSATIONS is off)")) };
    let before = q.get("before").and_then(|b| b.parse::<u64>().ok());
    // The page asked for without a `limit` is what each session's copy keeps in memory, whatever else of it is
    // spilled (goal 11, plan 01a1a87b2e56 step 5).
    let limit = q.get("limit").and_then(|l| l.parse::<usize>().ok()).unwrap_or(crate::harness::copies::EVENTS_PAGE).clamp(1, 1000);
    // From the session's copy when it holds them all: the events' texts as each was written once,
    // neither the store read nor anything encoded (goal 10, plan 01a1a87b2dae step 2), sent in the
    // pieces they're kept in, nothing inflated or joined for this read (step 5). On the request's shard
    // only as the copy keeps them: a compact chunk's texts no read kept yet are inflated on the blocking
    // pool by the read that needs them first, and kept for the reads after (step 5: inflated on the shard,
    // they were the lab's slowest read, a poll of `read /api/harness/sessions` 0.8 to 3.6 ms on the CPU).
    match hub.harness.events_page_kept(id, before, limit) {
        Ok(events) => return Ok(srv::response(StatusCode::OK, "application/json", srv::pieces(events))),
        Err(crate::harness::copies::Unread::Unkept) => {
            if let Some(view) = hub.harness.view_held(id) {
                match crate::rt::blocking(move || view.events_page(before, limit)).await {
                    Ok(Some(events)) => return Ok(srv::response(StatusCode::OK, "application/json", srv::pieces(events))),
                    // The copy can't say it after all (a chunk spilled meanwhile): the store's.
                    Ok(None) => {}
                    Err(e) => tracing::debug!(session = id, error = %format!("{e:#}"), "a page's texts weren't made off the shard: the store answers"),
                }
            }
        }
        Err(crate::harness::copies::Unread::Cannot) => {}
    }
    let id = id.to_owned();
    crate::reads::trace::asked();
    // The store's rows read, and written out as the answer, on the blocking pool: none of a page's JSON is read or
    // written on the request's shard (goal 10, plan 01a1a87b2dae step 5).
    let made = crate::rt::blocking(move || store.events(&id, before, limit).map(|rows| stored_events(&rows))).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let made = made.map_err(|e| err(StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")))?;
    made.map(pre_encoded).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("serialize: {e:#}")))
}

/// `GET /api/harness/sessions/{id}/events` as the conversation store's `rows` say it: `{events}`, each row's body read
/// as JSON (one that isn't is left out) and written out as `srv::json` writes an answer, compact.
fn stored_events(rows: &[crate::convo::rows::EventRow]) -> anyhow::Result<Bytes> {
    let events: Vec<serde_json::Value> = rows.iter().filter_map(|r| serde_json::from_str(&r.body).ok()).collect();
    crate::reads::encode(&json!({ "events": events }))
}

/// `GET /api/harness/sessions/:id/timeline?before=<seq>&lean=1`: the page of a session's events
/// just before number `before`, for a viewer whose window (or last page) begins there (docs/wire.md,
/// "Windows"): `{events, before?, more}`, oldest first, whole turns where they fit, at most a page's
/// bounds (`crate::harness::window`); `before` where it begins, to ask for the one before it. From
/// the server's copy of the session, and past what that holds, from its conversation store. Pages
/// asked for one after another meet with no gap and nothing twice: each is the events numbered
/// from where it begins up to where the last began, and new events are numbered after them all.
async fn harness_timeline(hub: &Hub, id: &str, q: &HashMap<String, String>) -> R {
    use crate::harness::registry::Paged;
    use crate::wire::ui::{EventsWindow, TimelinePage};
    let Some(before) = q.get("before").and_then(|b| b.parse::<u64>().ok()) else { return Err(err(StatusCode::BAD_REQUEST, "before is the number of the first event the viewer has")) };
    let lean = q.get("lean").is_some_and(|v| v == "1");
    let page = hub.harness.page(id, before, lean).await.map_err(|e| err(StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")))?;
    let page = match page.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such agent session"))? {
        Paged::Page { events, window } => TimelinePage { events: events.iter().filter_map(|t| serde_json::from_str(t).ok()).collect(), window },
        Paged::Store { before, floor } => {
            let Some(store) = hub.harness.convo.clone() else { return Ok(srv::json(StatusCode::OK, &TimelinePage { events: Vec::new(), window: EventsWindow::default() })) };
            let session = id.to_owned();
            // As many as a page holds at most, by number; the page is cut from them as the copy's are.
            let rows = crate::rt::blocking(move || store.events(&session, Some(before), crate::harness::window::ROWS)).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
            let rows = rows.map_err(|e| err(StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")))?;
            let read = rows.len();
            let events: Vec<serde_json::Value> = rows.iter().filter(|r| r.seq >= floor).filter_map(|r| serde_json::from_str::<serde_json::Value>(&r.body).ok()).map(|e| if lean { crate::harness::window::lean(&e).unwrap_or(e) } else { e }).collect();
            store_page(events, read, floor)
        }
    };
    Ok(srv::json(StatusCode::OK, &page))
}

/// A page cut from `events`, the newest the conversation store keeps before a viewer's (oldest
/// first, `read` of them asked for, the timeline beginning at `floor`): as many as a page holds,
/// whole turns where they fit. More before it when it left some of them out, or the store had
/// as many as were asked for (it may have more).
fn store_page(events: Vec<serde_json::Value>, read: usize, floor: u64) -> crate::wire::ui::TimelinePage {
    use crate::harness::window;
    let begin = window::start(&events, events.len(), window::PAGE_TURNS, |e| serde_json::to_string(e).map_or(0, |t| t.len()), window::begins_turn);
    let before = events.get(begin).and_then(|e| e.get("seq")).and_then(serde_json::Value::as_u64);
    let more = begin > 0 || (read >= window::ROWS && before.is_some_and(|s| s > floor));
    crate::wire::ui::TimelinePage { events: events.into_iter().skip(begin).collect(), window: crate::wire::ui::EventsWindow { before, more } }
}

/// `GET /api/harness/sessions/:id/events/:event?seq=`: one event of a session, whole: what a lean
/// one (`?lean=1` on its socket or its pages) held back, asked for when a person opens it. From the
/// server's copy, else (`seq`: its number) the conversation store.
async fn harness_event(hub: &Hub, id: &str, event: &str, q: &HashMap<String, String>) -> R {
    if let Some(text) = hub.harness.event_text(id, event).await.map_err(|e| err(StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")))? {
        let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        return Ok(srv::json(StatusCode::OK, &v));
    }
    let (Some(store), Some(seq)) = (hub.harness.convo.clone(), q.get("seq").and_then(|s| s.parse::<u64>().ok())) else { return Err(err(StatusCode::NOT_FOUND, "no such event")) };
    let session = id.to_owned();
    let rows = crate::rt::blocking(move || store.events(&session, Some(seq.saturating_add(1)), 1)).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let rows = rows.map_err(|e| err(StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")))?;
    let found = rows.into_iter().find(|r| r.id == event && r.seq == seq).ok_or_else(|| err(StatusCode::NOT_FOUND, "no such event"))?;
    let v: serde_json::Value = serde_json::from_str(&found.body).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(srv::json(StatusCode::OK, &v))
}

#[derive(Deserialize)]
struct RestoreFrom {
    from: String,
}

/// `POST /api/conversations/restore` `{from}`: ask that mirror for its copy of the conversation
/// store, and restore this one from it: what the copy brings fills in what the store lacks.
async fn conversation_restore(hub: &Arc<Hub>, req: Request) -> R {
    let b: RestoreFrom = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    hub.harness.restore_from(hub, b.from.trim()).map_err(|e| err(StatusCode::CONFLICT, format!("{e:#}")))?;
    Ok(srv::json(StatusCode::ACCEPTED, &hub.harness.restore_view()))
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

/// `GET /api/conversations/series?key=&window=`: store `key`'s series over the last `window`
/// seconds (3600 unless said; up to a day), a column each; none yet (a store the board hasn't
/// sampled since the server started): empty.
fn conversation_series(hub: &Hub, q: &HashMap<String, String>) -> R {
    let key = q.get("key").map(String::as_str).unwrap_or(crate::convo::board::SERVER);
    let window = q.get("window").and_then(|v| v.parse::<i64>().ok()).unwrap_or(3600).clamp(60, 86_400);
    let series = hub.harness.board.series(key, window, crate::util::now_ms()).unwrap_or_else(|| crate::convo::board::SeriesView { key: key.to_owned(), ..Default::default() });
    Ok(srv::json(StatusCode::OK, &series))
}

/// `GET /api/conversations/log?since=&q=&limit=`: the replication log, newest first: what moved
/// between the conversation stores, sample by sample, and what happened to them; after `since`
/// (unix ms), with `q` in the line (any case). `total`: every line it holds; `matched`: those with `q`.
fn conversation_log(hub: &Hub, q: &HashMap<String, String>) -> R {
    let since = q.get("since").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
    let limit = q.get("limit").and_then(|v| v.parse::<usize>().ok()).unwrap_or(1_000).clamp(1, 5_000);
    let words = q.get("q").map_or("", String::as_str);
    let board = &hub.harness.board;
    let (total, matched) = board.log_len(words);
    Ok(srv::json(StatusCode::OK, &json!({ "at": crate::util::now_ms(), "total": total, "matched": matched, "lines": board.log(since, words, limit) })))
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

/// Whether `caller` may change agent `id`'s goals: a person those of the agents they act on, an
/// agent its own and those of the agents it started (`crate::harness::lineage::may_steer`).
async fn may_steer_goals(hub: &Hub, caller: &crate::authz::Caller, id: &str) -> Result<(), ApiError> {
    crate::harness::lineage::may_steer(hub, caller, id).await.map_err(|(c, m)| err(c, m))
}

/// An agent's goals not yet started, as they should be (`crate::goals::set`).
async fn goals_set(hub: &Hub, caller: &crate::authz::Caller, id: &str, req: Request) -> R {
    may_steer_goals(hub, caller, id).await?;
    let b: GoalsSet = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let s = crate::goals::set(hub, id, b.goals).await.map_err(goals_err)?;
    hub.store.audit(caller.name.clone(), "goals.set".into(), id.to_owned()).await;
    Ok(srv::json(StatusCode::OK, &s))
}

/// Goals lined up after the agent's others (`crate::goals::add_at`): `{goals, start?, parallel?, key?}`,
/// each goal its words or `{text, needs}` (`crate::goals::ops::goals_of`). Its idempotency key
/// (CP-T3.2; `key`, or its `Idempotency-Key` header): asked again under it, the goals it added are
/// found, never added twice.
async fn goals_add(hub: &Hub, caller: &crate::authz::Caller, id: &str, req: Request) -> R {
    may_steer_goals(hub, caller, id).await?;
    let header = idempotency_key(&req);
    let b: serde_json::Value = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let by = crate::goals::By::of(hub, caller, id);
    let goals = crate::goals::ops::goals_of(&b, &by.name()).map_err(|m| err(StatusCode::BAD_REQUEST, m))?;
    let start = b.get("start").and_then(serde_json::Value::as_bool).unwrap_or(false);
    let key = match b.get("key") {
        None | Some(serde_json::Value::Null) => header,
        Some(serde_json::Value::String(k)) => Some(k.trim().to_owned()).filter(|k| !k.is_empty()).or(header),
        Some(_) => return Err(err(StatusCode::BAD_REQUEST, "`key` is a string")),
    };
    if key.as_ref().is_some_and(|k| k.len() > crate::harness::ops::KEY_MAX || k.chars().any(char::is_control)) {
        return Err(err(StatusCode::BAD_REQUEST, format!("a key is 1 to {} bytes with no control characters", crate::harness::ops::KEY_MAX)));
    }
    let goals = crate::goals::keyed(&crate::harness::ops::key_scope(caller), id, key.as_deref(), goals);
    let s = crate::goals::add_at(hub, id, goals, start, &crate::goals::At::End).await.map_err(goals_err)?;
    hub.store.audit(caller.name.clone(), "goals.add".into(), id.to_owned()).await;
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

/// One of a program's changes (`programs.set`, `programs.assign`): the request's JSON
/// object with `base` (the program, and the track) on it.
async fn programs_op(hub: &Arc<Hub>, caller: &crate::authz::Caller, name: &str, base: serde_json::Value, req: Request) -> R {
    let mut b: serde_json::Value = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let (Some(o), Some(base)) = (b.as_object_mut(), base.as_object()) else { return Err(err(StatusCode::BAD_REQUEST, "the request is a JSON object")) };
    o.extend(base.clone());
    own_op(hub, caller, name, b).await
}

/// One of kiln's own operations (agents, plans, goal stacks) as its HTTP route, which the route
/// table let through (and the audit trail records): `crate::ops::own`.
async fn own_op(hub: &Arc<Hub>, caller: &crate::authz::Caller, name: &str, input: serde_json::Value) -> R {
    let cx = crate::ops::Cx { hub: Arc::clone(hub), caller: caller.clone() };
    crate::ops::run_own(cx, name, input).await.map(|v| srv::json(StatusCode::OK, &v)).map_err(|(c, m)| err(c, m))
}

/// A query as an operation's input: each parameter a string.
fn query_value(q: &HashMap<String, String>) -> serde_json::Value {
    serde_json::Value::Object(q.iter().map(|(k, v)| (k.clone(), json!(v))).collect())
}

/// A request's JSON body as an operation's input (none: `{}`).
async fn body_value(req: Request) -> Result<serde_json::Value, ApiError> {
    let bytes = srv::body_bytes(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(json!({}));
    }
    match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(v) if v.is_object() => Ok(v),
        Ok(_) => Err(err(StatusCode::BAD_REQUEST, "the body is a JSON object")),
        Err(e) => Err(err(StatusCode::BAD_REQUEST, format!("the body isn't JSON: {e}"))),
    }
}

#[derive(Deserialize)]
struct HarnessOpen {
    target: String,
    spec: crate::harness::session::Spec,
    #[serde(default)]
    count: Option<usize>,
    /// The ids to start them under, one each, made by the client (`crate::util::is_id`), so its
    /// page can open the first one's own page at once (W21-R15); asked again under the same ids, a
    /// start is refused (409) rather than started twice.
    #[serde(default)]
    ids: Vec<String>,
}

async fn harness_open(hub: &Hub, req: Request) -> R {
    let caller = caller_of(hub, req.headers()).await;
    let mut body: HarnessOpen = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    // Ending a process found running is asked for with a take-over, never with a spec.
    body.spec.takeover_stop = false;
    // Who started an agent, the plan run it works for, and whether it's that agent's subagent are
    // kiln's to say (`agents.start`, `plans.run`, a goal stack's steps side by side), never what's
    // asked.
    body.spec.parent = None;
    body.spec.plan_run = None;
    body.spec.helper = None;
    body.spec.subagent = false;
    // A workspace of its own is asked for by its kind alone (its machine makes it), and only of a
    // machine that can make one: one that can't would let it work in the folder itself, unasked.
    match body.spec.workspace.as_ref().map(|w| w.kind.as_str()) {
        None | Some("folder") => body.spec.workspace = None,
        Some("volume") => {
            crate::harness::lineage::workspaces_on(hub, &body.target).await.map_err(|why| err(StatusCode::CONFLICT, format!("an agent can't have a workspace of its own there: {why}")))?;
            body.spec.workspace = Some(crate::workspace::Workspace { kind: "volume".into(), ..Default::default() });
        }
        Some(other) => return Err(err(StatusCode::BAD_REQUEST, format!("a workspace is \"folder\" or \"volume\", not {other:?}"))),
    }
    // Agents run programs as the machine's user, like terminals: the same switch governs both, and
    // the same right (docs/agents.md, "Whose agents").
    if body.target == "local" && !hub.cfg.terminal.enabled {
        return Err(err(StatusCode::FORBIDDEN, "terminals (and so agents) are disabled on this server"));
    }
    if let Some(c) = &caller {
        may_run_on(c, &body.target, "open an agent there")?;
    }
    match crate::harness::kinds::Kind::get(&body.spec.kind) {
        None => return Err(err(StatusCode::BAD_REQUEST, format!("unknown harness {:?}", body.spec.kind))),
        Some(k) if !k.structured() => {
            return Err(err(StatusCode::BAD_REQUEST, format!("{} has no structured mode: open a terminal and run `{}`", k.name, k.program)));
        }
        Some(_) => {}
    }
    let count = body.count.unwrap_or(1);
    if !body.ids.is_empty() {
        let n = count.clamp(1, 16);
        if body.ids.len() != n {
            return Err(err(StatusCode::BAD_REQUEST, format!("`ids` names one id for each agent it starts: {n}, not {}", body.ids.len())));
        }
        if let Some(bad) = body.ids.iter().find(|id| !crate::util::is_id(id)) {
            return Err(err(StatusCode::BAD_REQUEST, format!("{bad:?} isn't an id: an id is 32 lowercase hex digits")));
        }
        let mut seen = std::collections::HashSet::new();
        if !body.ids.iter().all(|id| seen.insert(id.as_str())) {
            return Err(err(StatusCode::BAD_REQUEST, "`ids` names each agent's id once"));
        }
    }
    hub.store.audit("admin".into(), "agent.open".into(), format!("{} on {}", body.spec.kind, body.target)).await;
    let infos = hub.harness.open_as(hub, &body.target, body.spec, count, &body.ids).await.map_err(|e| {
        let status = if e.downcast_ref::<crate::harness::registry::IdTaken>().is_some() { StatusCode::CONFLICT } else { StatusCode::BAD_REQUEST };
        err(status, format!("{e:#}"))
    })?;
    // Whom each acts for when its MCP server asks kiln for something (`crate::mcp::runner_ask`), and
    // whose it is.
    if let Some(c) = &caller {
        opened_by(hub, c, &infos).await;
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
/// The agents the request's caller acts on (docs/agents.md, "Whose agents"), what a broadcast picks among.
async fn acted_on(hub: &Hub, req: &Request) -> Vec<crate::harness::session::Info> {
    match req.extensions().get::<crate::authz::Caller>() {
        Some(c) => crate::harness::lineage::reached(hub, c, hub.harness.list(), crate::harness::lineage::Access::Act).await,
        None => hub.harness.list(),
    }
}

async fn harness_broadcast(hub: &Arc<Hub>, req: Request) -> R {
    let (header, by, among) = (idempotency_key(&req), key_scope(&req), acted_on(hub, &req).await);
    let mut b: crate::harness::broadcast::Broadcast = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    if b.key.is_none() {
        b.key = header;
    }
    let report = crate::harness::broadcast::run(hub, &b, &by, &among).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let what = b.text.as_deref().map_or_else(|| b.approvals.as_deref().map_or_else(|| "a command".to_owned(), |a| format!("{a} what waits")), |t| t.chars().take(80).collect());
    hub.store.audit("admin".into(), "agent.broadcast".into(), format!("{} to {} agents ({} sent, {} skipped, {} failed): {what}", report.id, report.results.len(), report.sent, report.skipped, report.failed)).await;
    Ok(srv::json(StatusCode::OK, &report))
}

/// Who a broadcast to `to` would reach (the same picking as the broadcast itself), the machines
/// they are on, and what they understand: their catalogs (slash commands, modes, models, efforts,
/// and the speeds past standard their models run at) merged, each entry with `count`, how many of
/// them have it, and `agents`, which.
async fn harness_broadcast_preview(hub: &Arc<Hub>, req: Request) -> R {
    #[derive(serde::Deserialize)]
    struct Body {
        to: crate::harness::broadcast::To,
    }
    let among = acted_on(hub, &req).await;
    let b: Body = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let picked = crate::harness::broadcast::pick(&among, &b.to);
    let mut machines: Vec<String> = Vec::new();
    // (field in each agent's meta, the key naming an entry, the entries seen with the agents that have them)
    let mut merged: [(&str, &str, Vec<(serde_json::Value, Vec<String>)>); 5] =
        [("commands", "name", Vec::new()), ("modes", "id", Vec::new()), ("models", "id", Vec::new()), ("efforts", "id", Vec::new()), ("speeds", "id", Vec::new())];
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
                    Some((_, ids)) => ids.push(info.id.clone()),
                    None => seen.push((entry.clone(), vec![info.id.clone()])),
                }
            }
        }
    }
    let counted = |seen: Vec<(serde_json::Value, Vec<String>)>| -> Vec<serde_json::Value> {
        seen.into_iter()
            .map(|(mut v, ids)| {
                if let Some(o) = v.as_object_mut() {
                    o.insert("count".into(), json!(ids.len()));
                    o.insert("agents".into(), json!(ids));
                }
                v
            })
            .collect()
    };
    let [(_, _, commands), (_, _, modes), (_, _, models), (_, _, efforts), (_, _, speeds)] = merged;
    let agents: Vec<serde_json::Value> = picked
        .iter()
        .map(|i| json!({"id": i.id, "name": i.name, "machine": i.target.strip_prefix("agent:").unwrap_or(&i.target), "kind": i.kind, "status": i.status}))
        .collect();
    Ok(srv::json(
        StatusCode::OK,
        &json!({"agents": agents, "machines": machines, "commands": counted(commands), "modes": counted(modes), "models": counted(models), "efforts": counted(efforts), "speeds": counted(speeds)}),
    ))
}

/// Whose a request's idempotency key is: the agent it comes from (if one) and the person it acts
/// for. Another caller's same key is another message (CP-T3.2).
fn key_scope(req: &Request) -> String {
    let caller = req.extensions().get::<crate::authz::Caller>().cloned().unwrap_or_else(crate::authz::Caller::owner);
    crate::harness::ops::key_scope(&caller)
}

/// A request's `Idempotency-Key` header, when it has one (CP-T3.2).
fn idempotency_key(req: &Request) -> Option<String> {
    req.headers().get("idempotency-key").and_then(|v| v.to_str().ok()).map(str::trim).filter(|k| !k.is_empty()).map(str::to_owned)
}

/// `POST /api/harness/sessions/:id/command`: one command for one session. A message's `id` is its
/// idempotency key, made once by its sender (or, with an `Idempotency-Key` header, made from that):
/// sent again, it's the same message, delivered once, and answered as it was the first time.
async fn harness_command(hub: &Arc<Hub>, id: &str, req: Request) -> R {
    let key = idempotency_key(&req);
    if key.as_ref().is_some_and(|k| k.len() > crate::harness::ops::KEY_MAX || k.chars().any(char::is_control)) {
        return Err(err(StatusCode::BAD_REQUEST, format!("an Idempotency-Key is 1 to {} bytes with no control characters", crate::harness::ops::KEY_MAX)));
    }
    let caller = req.extensions().get::<crate::authz::Caller>().cloned().unwrap_or_else(crate::authz::Caller::owner);
    let mut c: crate::harness::model::Command = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    if let Some(k) = &key {
        let made = crate::util::derive_id(&["api.command", &crate::harness::ops::key_scope(&caller), id, k]);
        match &mut c {
            crate::harness::model::Command::Send { id: mid, .. } => *mid = made,
            // Delivered like a message: through the session's inbox too.
            crate::harness::model::Command::SkillRun { id: mid, .. } => *mid = Some(made),
            _ => {}
        }
    }
    let message = crate::harness::registry::message_id(&c);
    // Any other command under a key is taken once (docs/wire.md, "The UI's sockets"): asked again
    // under it, it's answered as it was the first time. A message is its inbox's to take once.
    let sent = match key.filter(|_| !crate::harness::session::is_message(&c)) {
        Some(k) => {
            use crate::harness::asked::Taken;
            let scope = crate::harness::ops::key_scope(&caller);
            let kind = hub.harness.kind_of(hub, id, &c);
            let rx = match hub.harness.asked.take(id, &scope, &k, kind, Instant::now()) {
                (_, Taken::New(rx)) => {
                    hub.harness.send_asked(hub, id, &scope, &k, c).await;
                    rx
                }
                (_, Taken::Waiting(rx)) => rx,
                (_, Taken::Answered(sent)) => return answer_command(sent, message),
            };
            rx.await.unwrap_or_else(|_| crate::harness::registry::Sent::Unanswered("the server let go of it before its answer came".into()))
        }
        None => hub.harness.command(hub, id, c).await,
    };
    answer_command(sent, message)
}

/// The API's answer to a command: what became of it, as an HTTP status.
fn answer_command(sent: crate::harness::registry::Sent, message: Option<String>) -> R {
    match sent {
        crate::harness::registry::Sent::Ok | crate::harness::registry::Sent::Taken => Ok(srv::status(StatusCode::NO_CONTENT)),
        // Waits for its runner, which is away: it goes once the runner is back.
        crate::harness::registry::Sent::Parked => Ok(srv::json(StatusCode::ACCEPTED, &json!({"parked": true}))),
        crate::harness::registry::Sent::Accepted(a) => Ok(srv::json(StatusCode::OK, &crate::harness::registry::accepted(&message, &a))),
        crate::harness::registry::Sent::Refused(r) => {
            let (code, why) = crate::harness::ops::refused(&r);
            Err(err(code, why))
        }
        s @ (crate::harness::registry::Sent::Rejected { .. } | crate::harness::registry::Sent::Unanswered(_)) => {
            let (code, why) = s.failure().unwrap_or((StatusCode::CONFLICT, "its runner refused it".to_owned()));
            Err(err(code, why))
        }
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
    /// An idempotency key: sent again with it, each agent's message is the same one (CP-T3.2).
    #[serde(default)]
    key: Option<String>,
}

/// `POST /api/harness/command`: one command for the sessions listed, through the same sending as
/// `/api/harness/broadcast` (each send its own message id); `sent` lists those it reached, and the
/// rest of the report says why the others weren't.
async fn harness_command_many(hub: &Arc<Hub>, req: Request) -> R {
    use crate::harness::broadcast::{Broadcast, To};
    let (header, by, among) = (idempotency_key(&req), key_scope(&req), acted_on(hub, &req).await);
    let b: HarnessBroadcast = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    // Each agent's message is made from a key: the one given, else the message's own id.
    let key = b.key.clone().or(header).or_else(|| crate::harness::registry::message_id(&b.command));
    let command = serde_json::to_value(&b.command).map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let to = To { ids: b.ids, ..To::default() };
    let report = crate::harness::broadcast::run(hub, &Broadcast { to, text: None, attachments: Vec::new(), mode: None, command: Some(command), approvals: None, key }, &by, &among)
        .await
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let sent: Vec<&str> = report.results.iter().filter(|o| o.result == "sent").map(|o| o.id.as_str()).collect();
    Ok(srv::json(StatusCode::OK, &json!({ "sent": sent, "report": report })))
}

/// `POST /api/harness/sessions/:id/streams`: stream a page from the session's machine
/// (`crate::browser::stream::Spec`'s fields: url, width, height, scale, mobile, codec, bitrate).
/// Who asks, as a person, for what's theirs alone (a stream in their own profile): the owner, or a
/// user signed in as themselves; nobody for a job, an agent, or a token with a narrower grant.
fn person(req: &Request) -> Option<String> {
    match req.extensions().get::<crate::authz::Caller>() {
        None => Some("owner".to_owned()),
        Some(c) if c.agent.is_some() => None,
        Some(c) => c.opener(),
    }
}

/// Why this caller can't have a browser profile of its own (`None`: it can). A profile of one's
/// own keeps a person's sign-ins, so it's theirs alone: signed in as themselves, not through an
/// agent, a job, or a token allowed less than its owner.
fn personal_refusal(req: &Request) -> Option<String> {
    if person(req).is_some() {
        return None;
    }
    let as_whom = match req.extensions().get::<crate::authz::Caller>() {
        Some(c) if c.agent.is_some() => format!("an agent ({})", c.name),
        Some(c) if matches!(c.kind, crate::authz::gate::Kind::Job) => format!("a CI job ({})", c.name),
        Some(c) => format!("a token allowed less than its owner ({})", c.name),
        None => "someone kiln can't name".to_owned(),
    };
    Some(format!(
        "Your own profile keeps your sign-ins, so only you can use it, signed in to kiln as yourself. This browser is signed in as {as_whom}. Pick Conversation (kept with this conversation) or Clean (fresh each time), or sign in to kiln as yourself (your password, or the admin token)."
    ))
}

async fn stream_open(hub: &Arc<Hub>, id: &str, req: Request) -> R {
    let by = req.extensions().get::<crate::authz::Caller>().map_or_else(|| "owner".to_owned(), |c| c.name.clone());
    let me = person(&req);
    let refusal = personal_refusal(&req);
    let mut spec: serde_json::Value = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    if spec.get("profile").and_then(serde_json::Value::as_str) == Some("personal")
        && let Some(why) = refusal
    {
        return Err(err(StatusCode::FORBIDDEN, why));
    }
    // `{"start": true}`: the project's app first (its `.kiln/preview.yaml`), then its page.
    if spec.get("start").and_then(serde_json::Value::as_bool) == Some(true) {
        let started = crate::harness::streams::preview(hub, id, "start").await.map_err(|e| err(StatusCode::CONFLICT, format!("{e:#}")))?;
        if started.get("started").and_then(serde_json::Value::as_bool) == Some(true) {
            hub.store.audit("admin".into(), "agent.app.start".into(), format!("{id}: {}", started.get("url").and_then(serde_json::Value::as_str).unwrap_or(""))).await;
        }
        if let (Some(o), Some(url)) = (spec.as_object_mut(), started.get("url").cloned()) {
            o.insert("url".into(), url);
        }
    }
    let profile = crate::harness::streams::profile_for(id, spec.get("profile").and_then(serde_json::Value::as_str), me.as_deref()).map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    if let Some(o) = spec.as_object_mut() {
        o.insert("profile".into(), serde_json::json!(profile));
        // A phone's size until its viewer says its own (a runner from before needs one).
        o.entry("width").or_insert(serde_json::json!(390));
        o.entry("height").or_insert(serde_json::json!(640));
    }
    let live = hub.harness.streams.open(hub, id, spec, &by).await.map_err(|e| err(StatusCode::CONFLICT, format!("{e:#}")))?;
    let kind = profile.split(':').next().unwrap_or("conversation");
    hub.store.audit("admin".into(), "agent.stream.open".into(), format!("{id}: {} ({kind} profile)", live.spec.get("url").and_then(serde_json::Value::as_str).unwrap_or(""))).await;
    Ok(srv::json(StatusCode::CREATED, &live.view()))
}

/// `POST /api/harness/sessions/:id/opened` with `{port, path}`: the app on that port of the
/// session's machine, opened on the person's own devices: a link at pages' own site, good once for
/// ten minutes (`crate::harness::opened`). Only with `[server] pages_url`: kiln never serves an app
/// from its own address, where it would have the person's kiln session.
async fn open_on_device(hub: &Arc<Hub>, id: &str, req: Request) -> R {
    let Some(site) = hub.cfg.server.pages_url.as_deref().map(|u| u.trim().trim_end_matches('/').to_owned()).filter(|u| !u.is_empty()) else {
        return Err(err(StatusCode::CONFLICT, "opening an app on your device needs pages' own site: set [server] pages_url (docs/previews.md). kiln never serves an app from its own address, where the app would have your kiln session"));
    };
    let by = req.extensions().get::<crate::authz::Caller>().map_or_else(|| "owner".to_owned(), |c| c.name.clone());
    let me = person(&req);
    let b: serde_json::Value = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let port = b.get("port").and_then(serde_json::Value::as_u64).and_then(|p| u16::try_from(p).ok()).filter(|p| *p > 0).ok_or_else(|| err(StatusCode::BAD_REQUEST, "which port? (`port`)"))?;
    let path = b.get("path").and_then(serde_json::Value::as_str).unwrap_or("/");
    let machine = match hub.harness.location(id) {
        Some(crate::harness::registry::Location::Machine(m)) => m,
        Some(crate::harness::registry::Location::Local) => return Err(err(StatusCode::CONFLICT, "apps open on devices from runners: this session runs on the server itself")),
        None => return Err(err(StatusCode::NOT_FOUND, "no such agent session")),
    };
    let caps = hub.routed_caps(&machine, &crate::proto::ServerMsg::PortOpen { conn: 0, session: id.to_owned(), port }).await;
    if caps & crate::proto::CAP_PORTS == 0 {
        return Err(err(StatusCode::CONFLICT, format!("{} runs a kiln too old to open its ports: upgrade its runner", hub.shown(&machine))));
    }
    let (o, code) = hub.harness.opened.create(id, port, path, &by, me);
    hub.store.audit("admin".into(), "agent.opened.create".into(), format!("{id}: port {port}")).await;
    let mut v = serde_json::to_value(&o).unwrap_or_default();
    if let Some(m) = v.as_object_mut() {
        m.insert("url".into(), serde_json::json!(format!("{site}/o/{code}")));
    }
    Ok(srv::json(StatusCode::CREATED, &v))
}

/// `GET /api/harness/sessions/:id/preview`: what a person may open live from the session (the
/// ports its machine serves, its agent's first; its project's app); `POST .../preview/stop` stops
/// that app.
async fn preview_look(hub: &Arc<Hub>, id: &str, op: &str, personal_refused: Option<String>) -> R {
    let mut v = crate::harness::streams::preview(hub, id, op).await.map_err(|e| err(StatusCode::CONFLICT, format!("{e:#}")))?;
    if op == "stop" && v.get("stopped").and_then(serde_json::Value::as_bool) == Some(true) {
        hub.store.audit("admin".into(), "agent.app.stop".into(), id.to_owned()).await;
    }
    // Whether the one asking may have a profile of their own, and why not (the form says so
    // before they choose it).
    if op == "look"
        && let Some(o) = v.as_object_mut()
    {
        o.insert("personal".into(), serde_json::json!({"ok": personal_refused.is_none(), "why": personal_refused}));
    }
    Ok(srv::json(StatusCode::OK, &v))
}

/// `GET /api/harness/sessions/:id/pages/:page/heights`: a page the session showed, its heights:
/// `heights` as its machine's Chromium measured them at each width, `engines` as other engines did
/// (its machine's Firefox; viewers' own browsers at their widths).
async fn page_heights(hub: &Arc<Hub>, session: &str, page: &str) -> R {
    let (h, id) = (hub.clone(), page.to_owned());
    let v = crate::rt::blocking(move || h.harness.visuals.get(&id)).await.ok().flatten().filter(|v| v.session == session).ok_or_else(|| err(StatusCode::NOT_FOUND, "no such page"))?;
    Ok(srv::json(StatusCode::OK, &serde_json::json!({"heights": v.heights, "engines": v.engines})))
}

/// `POST /api/harness/sessions/:id/pages/:page/heights` with `{engine, width, height}`: the height
/// a viewer's own browser measured the page at, at its width (`crate::harness::visuals::Visuals::seen`).
async fn page_seen(hub: &Arc<Hub>, session: &str, page: &str, req: Request) -> R {
    let b: serde_json::Value = srv::body_json(req.into_body(), JSON_LIMIT).await.map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let n = |k: &str| b.get(k).and_then(serde_json::Value::as_u64).and_then(|v| u32::try_from(v).ok()).unwrap_or(0);
    let (width, height) = (n("width"), n("height"));
    let engine = b.get("engine").and_then(serde_json::Value::as_str).unwrap_or("").to_owned();
    let (h, id, s) = (hub.clone(), page.to_owned(), session.to_owned());
    crate::rt::blocking(move || h.harness.visuals.seen(&id, &s, &engine, width, height))
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

/// `GET /api/harness/sessions/:id/streams/:stream/ws`: a viewer of a stream. It's sent the
/// stream's chunks (binary, as its runner sent them: `crate::browser::stream::frame`) and events
/// (text, JSON); it sends `{"input": [...]}` (what its person does) and `{"control": {...}}`
/// (resize, pause, keyframe, go, reload, back, forward).
///
/// With the server's WebRTC on (`crate::harness::webrtc`) it's told `{"webrtc": {"can": true}}`,
/// and may send `{"webrtc": {"offer": sdp}}`, answered `{"webrtc": {"answer": sdp}}` (or
/// `{"error"}`); once its peer shows the stream it sends `{"webrtc": {"playing": true}}` and the
/// chunks go to the peer alone, until it says `{"playing": false}` or `{"close": true}`, or the
/// peer goes (`{"webrtc": {"gone": why}}`).
fn stream_ws(hub: Arc<Hub>, id: &str, sid: &str, req: Request) -> R {
    let me = person(&req);
    let live = hub.harness.streams.get(sid).filter(|l| l.session == id && l.for_whom(me.as_deref())).ok_or_else(|| err(StatusCode::NOT_FOUND, "no such stream"))?;
    // What it can't read shows on its agent's page.
    let sock = ui::Socket::of(Wire::StreamUp, Wire::StreamDown, &caller_name(&req), Some(Page::Session(id.to_owned())));
    let (resp, upgrade) = ws::accept_limited(req, 1 << 20).map_err(|r| ApiError(r.status, r.reason.into()))?;
    crate::rt::spawn(async move {
        let Ok(socket) = upgrade.await else { return };
        if let Err(e) = stream_view(hub, live, socket, sock).await {
            tracing::debug!(error = %format!("{e:#}"), "stream websocket ended");
        }
    });
    Ok(resp)
}

async fn stream_view(hub: Arc<Hub>, live: Arc<crate::harness::streams::Live>, socket: ws::WebSocket, sock: ui::Socket) -> anyhow::Result<()> {
    use crate::harness::streams::ToViewer;
    use crate::harness::webrtc::{Peer, Said};
    use futures::FutureExt as _;
    /// A key frame asked for a peer's loss at most this often.
    const KEY_EVERY: std::time::Duration = std::time::Duration::from_millis(500);
    enum Next {
        Socket(Option<Result<Message, tungstenite::Error>>),
        Said(Option<Said>),
        Stream(Result<ToViewer, async_channel::RecvError>),
    }
    let (mut tx, mut rx) = socket.split();
    let rtc = &hub.harness.webrtc;
    // Said first, before joining (which waits on the stream's runner): a viewer that can show the
    // stream only over WebRTC knows at once whether it can.
    if rtc.on() {
        tx.send(ws::text(StreamDown::Webrtc { webrtc: WebrtcDown::Can(true) }.text())).await?;
    }
    let (me, from_stream) = hub.harness.streams.join(&hub, &live).await;
    // The viewer's WebRTC peer, and whether it shows the stream (its chunks then go there alone).
    let mut peer: Option<Peer> = None;
    let mut playing = false;
    let mut keyed: Option<std::time::Instant> = None;
    let r: anyhow::Result<()> = async {
        loop {
            let next = {
                let said = async {
                    match &peer {
                        Some(p) => p.said.recv().await.ok(),
                        None => futures::future::pending().await,
                    }
                };
                futures::select_biased! {
                    m = rx.next().fuse() => Next::Socket(m),
                    s = said.fuse() => Next::Said(s),
                    m = from_stream.recv().fuse() => Next::Stream(m),
                }
            };
            match next {
                Next::Socket(m) => {
                    let read = match ui::came(m) {
                        Came::Text(t) => match ui::stream_up(t.as_str()) {
                            Ok(Up::Frame(up)) => Ok(up),
                            // A frame that came down that the page couldn't read: noted.
                            Ok(Up::Unread(f)) => {
                                hub.note_told(&sock, &f).await;
                                continue;
                            }
                            Err(r) => Err(r),
                        },
                        Came::Binary(_) => Err(ui::Refused::binary()),
                        Came::Control => continue,
                        Came::Gone => return Ok(()),
                        Came::Ends { close, refused } => {
                            hub.note_refused(&sock, &refused).await;
                            let _ = tx.close(Some(close)).await;
                            return Ok(());
                        }
                    };
                    let up = match read {
                        Ok(up) => up,
                        Err(refused) => {
                            hub.note_refused(&sock, &refused).await;
                            tx.send(ws::text(StreamDown::Refused { refused }.text())).await?;
                            continue;
                        }
                    };
                    if let Some(events) = up.input {
                        live.input(&hub, &serde_json::Value::Array(events)).await;
                    }
                    if let Some(c) = up.control {
                        live.control(&hub, &serde_json::Value::Object(c)).await;
                    }
                    let Some(w) = up.webrtc else { continue };
                    let was = playing;
                    if let Some(sdp) = w.offer.as_deref() {
                        // A new peer, in place of one there was.
                        peer = None;
                        playing = false;
                        let webrtc = match rtc.offer(&live.codec(), sdp) {
                            Ok((p, answer)) => {
                                peer = Some(p);
                                WebrtcDown::Answer(answer)
                            }
                            Err(e) => WebrtcDown::Error(format!("{e:#}")),
                        };
                        tx.send(ws::text(StreamDown::Webrtc { webrtc }.text())).await?;
                    }
                    if let Some(on) = w.playing {
                        playing = on && peer.is_some();
                    }
                    if w.close == Some(true) {
                        peer = None;
                        playing = false;
                    }
                    // Back on this socket: its decoder starts again from a key frame.
                    if was && !playing {
                        live.control(&hub, &serde_json::json!({"keyframe": true})).await;
                    }
                }
                // Connected, it starts from a key frame; one lost, a key frame mends it.
                Next::Said(Some(Said::Connected)) => {
                    keyed = Some(std::time::Instant::now());
                    live.control(&hub, &serde_json::json!({"keyframe": true})).await;
                }
                Next::Said(Some(Said::Keyframe)) => {
                    if keyed.is_none_or(|k| k.elapsed() >= KEY_EVERY) {
                        keyed = Some(std::time::Instant::now());
                        live.control(&hub, &serde_json::json!({"keyframe": true})).await;
                    }
                }
                Next::Said(gone) => {
                    let why = match gone {
                        Some(Said::Gone(why)) => why,
                        _ => "WebRTC stopped on this server".to_owned(),
                    };
                    peer = None;
                    if playing {
                        playing = false;
                        live.control(&hub, &serde_json::json!({"keyframe": true})).await;
                    }
                    tx.send(ws::text(StreamDown::Webrtc { webrtc: WebrtcDown::Gone(why) }.text())).await?;
                }
                Next::Stream(Ok(ToViewer::Chunk(b))) => {
                    if let Some(p) = &peer {
                        p.chunk(&b);
                    }
                    if !playing {
                        tx.send(ws::binary(b)).await?;
                    }
                }
                Next::Stream(Ok(ToViewer::Event(e))) => tx.send(ws::text(e)).await?,
                Next::Stream(Err(_)) => return Ok(()),
            }
        }
    }
    .await;
    drop(peer);
    hub.harness.streams.leave(&hub, &live, me).await;
    r
}

/// After a snapshot on an agent session's socket: what was resent is complete.
fn synced() -> String {
    SessionDown::from(SessionFrame::Synced).text()
}

/// What a viewer of an agent session takes, as its socket's query says.
#[derive(Debug, Clone, Copy, Default)]
struct Takes {
    /// `z=gzip`: each snapshot packed. What a long conversation sends on attach is most of what it
    /// ever sends, and on a slow link a tenth of it arrives ten times sooner.
    packed: bool,
    /// `patch=state`: a state as only what changed in it (`state_patch`), after the snapshot's;
    /// `patch=changes`: a changeset so (`changes_patch`).
    patches: crate::harness::registry::Patches,
    /// `after=<born>.<rev>`: it had the session through that revision of this server's copy
    /// (docs/wire.md, "Revisions"), and gets only what it lacks when the copy can say.
    after: Option<(u64, u64)>,
    /// `window=1`: its first load the newest events alone, as many as a window holds; `lean=1`:
    /// settled tool calls without their bodies (docs/wire.md, "Windows").
    view: crate::harness::registry::View,
}

impl Takes {
    fn of(query: Option<&str>) -> Self {
        let has = |p: &str| query.is_some_and(|q| q.split('&').any(|x| x == p));
        let after = query
            .and_then(|q| q.split('&').find_map(|x| x.strip_prefix("after=")))
            .and_then(|v| v.split_once('.'))
            .and_then(|(b, r)| Some((b.parse().ok()?, r.parse().ok()?)));
        Self {
            packed: has("z=gzip"),
            patches: crate::harness::registry::Patches { state: has("patch=state"), changes: has("patch=changes") },
            after,
            view: crate::harness::registry::View { window: has("window=1"), lean: has("lean=1") },
        }
    }
}

fn harness_ws(hub: Arc<Hub>, id: String, req: Request) -> R {
    let takes = Takes::of(req.uri().query());
    let caller = req.extensions().get::<crate::authz::Caller>().cloned();
    // Whose its commands' ids are: a caller's, as an Idempotency-Key's are.
    let scope = caller.as_ref().map_or_else(String::new, crate::harness::ops::key_scope);
    let (resp, upgrade) = ws::accept_commands(req).map_err(|r| ApiError(r.status, r.reason.into()))?;
    // Served on its session's owner's shard, where the session's frames are published (goal 10, plan 01a1a87b2dae step
    // 5): a frame then reaches this viewer in its owner's own thread once the owner's turn ends, where a viewer on
    // another shard (its connection's, dealt round-robin: three in four with four shards) was woken across threads for
    // each, its shard kicked where it was parked, and behind a busy CPU up to a tick on a kernel that preempts lazily
    // (`crate::route_latency::FrameLeg::WokenOther`). Its connection moves there with its upgrade: a socket's waits are
    // one-shot, armed on whichever shard awaits it, and one left armed where it was is withdrawn there
    // (`hyper_rt::readiness`), as an accepted connection moves to the shard it's dealt.
    crate::rt::spawn_owner(hub.harness.owner_shard(&id), async move {
        let Ok(socket) = upgrade.await else { return };
        // Watching is seeing (checked before the upgrade); commands on the socket need acting, and
        // each is audited as its caller.
        let acts = match &caller {
            Some(c) => crate::harness::lineage::access(&hub, c, &id).await == crate::harness::lineage::Access::Act,
            None => false,
        };
        let who = caller.map_or_else(|| "admin".to_owned(), |c| c.name);
        if let Err(e) = harness_attach(hub, id, socket, takes, scope, acts, who).await {
            tracing::debug!(error = %format!("{e:#}"), "agent session websocket ended");
        }
    });
    Ok(resp)
}

/// The most bytes of a packed snapshot the blocking pool packs at a time (a piece, `Packing::next_piece`), each
/// piece's making a job of its own: what a frame carried, as much as tungstenite's writer gathered before it
/// wrote (its default `write_buffer_size`; goal 10, plan 01a1a87b2dae step 3), kept as the pool's unit so a
/// snapshot takes as many of its jobs as it did. Its frames are [`SNAPSHOT_FRAME`]s.
const SNAPSHOT_PIECE: usize = 128 * 1024;

/// The most bytes of a packed snapshot one WebSocket frame carries: what its socket writes at a time
/// ([`ws::SOCKET_WRITE`]). The socket's writer copies a frame into its buffer whole before it writes any of it,
/// on the shard that serves the socket, and a long session's snapshot is megabytes: a frame at a time goes, and
/// the shard serves its other sockets between. The writer's buffer keeps the most it ever held while the socket
/// is open, so a frame of a whole piece (128 KiB) kept 131 KiB for every viewer after its snapshot (goal 11,
/// plan 01a1a87b2e56 step 5).
const SNAPSHOT_FRAME: usize = ws::SOCKET_WRITE;

/// `bytes` as one binary message in frames of at most [`SNAPSHOT_FRAME`] bytes (RFC 6455 5.4's
/// fragments, which every client joins into the one message they are).
#[cfg(test)]
fn pieces(bytes: Vec<u8>) -> impl Iterator<Item = ws::Message> {
    pieces_of(bytes, true, true)
}

/// `bytes`, a piece of one binary message, in frames of at most [`SNAPSHOT_FRAME`] bytes: the message's
/// first frame (when `first`) binary and the rest continuations, its last (when `last`) final (RFC 6455
/// 5.4), so the pieces of a message packed as it goes out make one message (goal 11, plan 01a1a87b2e56 step 5).
fn pieces_of(bytes: Vec<u8>, first: bool, last: bool) -> impl Iterator<Item = ws::Message> {
    use tungstenite::protocol::frame::Frame;
    use tungstenite::protocol::frame::coding::{Data, OpCode};
    let bytes = bytes::Bytes::from(bytes);
    let n = bytes.len().div_ceil(SNAPSHOT_FRAME).max(1);
    (0..n).map(move |i| {
        let piece = bytes.slice((i * SNAPSHOT_FRAME).min(bytes.len())..((i + 1) * SNAPSHOT_FRAME).min(bytes.len()));
        let op = if i == 0 && first { Data::Binary } else { Data::Continue };
        ws::Message::Frame(Frame::message(piece, OpCode::Data(op), last && i + 1 == n))
    })
}

/// A snapshot's next piece as it's packed going out ([`crate::harness::registry::Packing`]), made on the
/// blocking pool: a place held for the piece, not for the viewer's reading (goal 11, plan 01a1a87b2e56 step 5).
/// Once it's all out (or its making failed), the snapshot is let go of there too, not on the viewer's shard: it
/// may be the last to hold what its copy has since let go of (the chunks, meta and state it was taken with),
/// and a long session's are thousands of allocations to free (goal 10, plan 01a1a87b2dae step 5).
async fn next_piece(mut packing: crate::harness::registry::Packing) -> anyhow::Result<(Option<crate::harness::registry::Packing>, Option<Vec<u8>>)> {
    let (packing, piece) = crate::rt::blocking(move || {
        let piece = packing.next_piece(SNAPSHOT_PIECE);
        let rest = matches!(piece, Ok(Some(_))).then_some(packing);
        (rest, piece)
    })
    .await?;
    Ok((packing, piece?))
}

/// `held` let go of on the blocking pool, not on the shard that awaits this: what's left of a snapshot may be the last
/// to hold what its copy has since let go of, as [`next_piece`] says (goal 9, plan 01a1a87b2cec step 3).
async fn let_go<T: Send + 'static>(held: T) {
    let _ = crate::rt::blocking(move || drop(held)).await;
}

/// Send a viewer a snapshot: its frames then `synced`, a message each; or, `packed`, all of it in
/// one gzip'd binary message, a frame a line (frames are JSON, whose strings escape their line
/// breaks). Written out on the blocking pool: a long session's is megabytes of JSON, and the
/// runtime's threads serve every socket (`crate::harness::registry::Snapshot`). Not on the compute
/// pool, below the shards: there a snapshot took 2.4 times as long with a busy process beside the
/// server for each of its CPUs, and the frames its viewer waits on with it 4.7 times (goal 4's lab,
/// plan 01a1a59c2a9c step 5).
async fn send_snapshot(tx: &mut ws::Sender, snapshot: crate::harness::registry::Snapshot, packed: bool) -> anyhow::Result<()> {
    // Room in the snapshots in flight first (`crate::harness::registry::snapshot_room`; goal 11, plan
    // 01a1a87b2e56 step 3): past it this attach waits its turn, until one in flight is written out. Made, it's
    // counted as it is, until handed to the socket: a packed one until its last piece goes (its pieces share
    // its buffer), frames as each goes.
    // Between its frames the shard goes to its other tasks: a socket that takes each piece at once never made it
    // wait, and a long session's megabytes went out in one poll, holding its shard's other viewers, owners and
    // requests 4.4 ms at the most in rt5's calm lab and 8.6 ms with shedding on (goal 10, plan 01a1a87b2dae step 5).
    // Its turn ends once its next frame would pass the shard's quantum, whether or not anything else wants the shard
    // then (`crate::rt::Turns::end`, a yield that always yields): a turn given up only where the shard is wanted
    // (`crate::rt::consume_budget`) goes on past the quantum where it isn't, and the snapshot's longest polls were
    // 2.4 to 3.5 ms on the CPU in rt9's calm rounds (goal 9, plan 01a1a87b2cec step 3: no poll past the quantum).
    // Its weight is reckoned on the blocking pool too, where its pieces are made: a whole snapshot's adds up every
    // chunk's events' lengths, a meta each (20,000 for the two longest sessions of kiln-lab's realistic profile, among
    // the eight at work that its viewers open three times in four), read on the viewer's shard in the snapshot's first
    // poll (step 3: `viewer snapshot` polls of 0.65 to 2.4 ms on the CPU in every round of g9p16's with hogs, train 16).
    let (snapshot, weight) = crate::rt::blocking(move || {
        let weight = snapshot.weight(packed);
        (snapshot, weight)
    })
    .await?;
    let mut held = crate::harness::registry::InFlight::take(crate::harness::registry::snapshot_room(), weight).await;
    if packed {
        // Packed as it goes out (goal 11, plan 01a1a87b2e56 step 5): a piece made, then the next made while
        // it's held (so its last frame is known final), then written; what's in flight is those two, not the
        // whole message, and a place of the blocking pool is held for each piece's making, not for the
        // viewer's reading.
        let (mut packing, mut now) = next_piece(snapshot.packing(&synced())).await?;
        let mut first = true;
        while let Some(piece) = now {
            let (rest, following) = match packing.take() {
                Some(p) => next_piece(p).await?,
                None => (None, None),
            };
            packing = rest;
            held.made(piece.capacity().saturating_add(following.as_ref().map_or(0, Vec::capacity)));
            let last = following.is_none();
            // A turn from here: the piece came in a poll of its own, made on the blocking pool.
            let mut turns = crate::rt::Turns::new();
            for frame in pieces_of(piece, first, last) {
                if let Err(e) = tx.send(frame).await {
                    // Its viewer gone before its last piece: what's left of it let go of on the blocking pool, as one
                    // all out is (`next_piece`), not on the viewer's shard (goal 9, plan 01a1a87b2cec step 3).
                    let_go(packing).await;
                    return Err(e.into());
                }
                turns.end().await;
            }
            first = false;
            now = following;
        }
        return Ok(());
    }
    let frames = crate::rt::blocking(move || snapshot.frames()).await?;
    held.made(frames.iter().map(String::capacity).fold(0usize, usize::saturating_add));
    let mut turns = crate::rt::Turns::new();
    for f in frames {
        let n = f.capacity();
        tx.send(ws::text(f)).await?;
        held.written(n);
        turns.end().await;
    }
    tx.send(ws::text(synced())).await?;
    Ok(())
}

/// A socket's audit rows waiting for the store, at most: past that, its next command waits for room
/// (a row is never dropped).
const AUDITS_WAITING: usize = 256;

/// Writes an agent session socket's audit rows of its commands (docs/ci.md W19-R05) in the order
/// they came, beside the socket: a command never waits for the store, whose one connection a busy
/// disk's checkpoint or another caller's long write holds for seconds. A message once waited so
/// before it went, and showed seconds late. What's waiting when the socket closes is still written.
fn audits(store: &crate::store::Store, who: String) -> crate::memory::queue::Sender<String> {
    // Lock-free (goal 4, plan 01a1a59c2a9c step 5): an async-channel's wait took event-listener's `Mutex`, the
    // socket's commands on its shard and the writer waiting on its own.
    let (tx, rx) = crate::memory::queue::places::<String>(&crate::memory::owners::CHANNELS, AUDITS_WAITING);
    let store = store.clone();
    crate::rt::spawn(async move {
        while let Ok(detail) = rx.recv().await {
            store.audit(who.clone(), "agent.command".into(), detail).await;
        }
    });
    tx
}

/// A socket command's row of its own (the socket's request is a GET), as its caller: which command,
/// not what it says. It goes to the socket's writer of rows (`audits`, made at its first command),
/// so the command goes on without waiting for the store; should that writer be gone, it's written
/// here.
async fn audit_command(hub: &Hub, audit: &mut Option<crate::memory::queue::Sender<String>>, who: &str, id: &str, c: &crate::harness::model::Command) {
    let kind = serde_json::to_value(c).ok().and_then(|v| v.get("type").and_then(serde_json::Value::as_str).map(str::to_owned)).unwrap_or_default();
    let writer = audit.get_or_insert_with(|| audits(&hub.store, who.to_owned()));
    if let Err(async_channel::SendError(row)) = writer.send(format!("{id}: {kind}")).await {
        hub.store.audit(who.to_owned(), "agent.command".into(), row).await;
    }
}

/// What a viewer that may watch an agent but not command it is told of a command.
const WATCHING: &str = "you may watch this agent but not command it: an agent commands only itself and the agents it started";

/// The frame that answers command `cmd` (of `kind`) once `rx` says what became of it.
fn answer_of(cmd: String, rx: futures::channel::oneshot::Receiver<crate::harness::registry::Sent>, kind: crate::harness::asked::Kind) -> futures::future::BoxFuture<'static, Option<String>> {
    use futures::FutureExt as _;
    async move {
        let sent = rx.await.unwrap_or_else(|_| crate::harness::registry::Sent::Unanswered("the server let go of it before its answer came".into()));
        Some(crate::harness::asked::frame(&cmd, &sent, kind))
    }
    .boxed()
}

/// What a viewer is told of a command that didn't get through: refused by its session's inbox (a
/// message is never dropped without a word), or why it didn't reach it. Nothing when it went.
fn refusal_frame(sent: crate::harness::registry::Sent) -> Option<String> {
    use crate::harness::registry::Sent;
    match sent {
        Sent::Ok | Sent::Accepted(_) | Sent::Taken | Sent::Parked => None,
        Sent::Refused(r) => Some(SessionDown::from(SessionFrame::Error { error: r.to_string(), refused: Some(r) }).text()),
        s @ (Sent::Rejected { .. } | Sent::Unanswered(_)) => s.failure().map(|(_, why)| session_error(why)),
        Sent::NoSession => Some(session_error("this session is gone".into())),
        Sent::Unreachable => Some(session_error("the session's machine is not connected".into())),
    }
}

/// An `error` frame on a session's socket, as clients from before ids are told.
fn session_error(error: String) -> String {
    SessionDown::from(SessionFrame::Error { error, refused: None }).text()
}

/// Live frames a viewer reads from its session's feed at once: at most this many of the bus's
/// largest messages (1 MiB) held for it at a time.
const FEED_READ: usize = 16;

/// How long a viewer with nothing to read waits before it looks whether its feed broke (a full bus
/// may refuse the resync marker as well as its frames), and how long a viewer of a session whose
/// feed the bus can't hold waits between snapshots. Under 5 s, so a viewer whose feed broke starts
/// over within 5 s of it, its wait having begun at most a frame's handling after.
const FEEDLESS: std::time::Duration = std::time::Duration::from_secs(4);

/// What a viewer's wait on its session's feed came to.
enum Live {
    /// A frame or more to read.
    Ready,
    Refused(kiln_seams::bus::Refusal),
    /// The session has no feed on the bus: time for another snapshot.
    Feedless,
    /// A while with nothing: time to see whether the feed broke since (a full bus may refuse its
    /// resync marker as well as its frames).
    Quiet,
}

/// `fut`'s output, and whether it was waited for (a poll of it was pending, so its task gave its shard up before it
/// came): what follows it then begins a turn of its own on the shard (`crate::rt::Turns`; goal 9, plan 01a1a87b2cec
/// step 3).
async fn waited<F: Future>(fut: F) -> (F::Output, bool) {
    let mut fut = std::pin::pin!(fut);
    let mut pending = false;
    let out = std::future::poll_fn(|cx| {
        let polled = fut.as_mut().poll(cx);
        pending |= polled.is_pending();
        polled
    })
    .await;
    (out, pending)
}

/// A viewer's take of a batch of its session's feed ([`write_live_batch`]; goal 10, plan 01a1a87b2dae step 5): when its
/// task's poll found the batch there (`woke_us`) and read it off the bus (`now_us`), on the clock each frame's publish is
/// stamped by ([`crate::harness::live::now_us`]), the read on the monotonic clock its write is timed from (`at`), and
/// the leg its frames' wait for that poll was: `woken` with the viewer on its session's owner's shard, `woken_other`
/// across shards (`crate::route_latency::FrameLeg`).
#[derive(Clone, Copy, Debug)]
struct Read {
    woke_us: u64,
    now_us: u64,
    at: Instant,
    woken: crate::route_latency::FrameLeg,
}

impl Read {
    /// A batch read now, its viewer's task woken at `woke_us`, on its session's owner's shard (`own`) or another.
    fn now(woke_us: u64, own: bool) -> Self {
        let woken = if own { crate::route_latency::FrameLeg::Woken } else { crate::route_latency::FrameLeg::WokenOther };
        Self { woke_us, now_us: crate::harness::live::now_us(), at: Instant::now(), woken }
    }
}

/// What a batch of a session's feed came to for its viewer ([`write_live_batch`]).
#[derive(Debug, PartialEq, Eq)]
enum Fed {
    /// Its frames went out: the next read goes on from the batch's end.
    Written,
    /// A frame the feed didn't make, its resync marker, or a frame the viewer can't take: the viewer starts
    /// over, the batch's frames before it gone out.
    StartOver,
    /// The session's last frame went out: the viewer ends with it.
    Closed,
}

/// Writes a batch of a session's feed to its viewer's socket `tx` (goal 9, plan 01a1a87b2cec step 5): each frame
/// the viewer takes is fed to the socket and the batch flushed once (and at the end of each of its turns on the
/// shard, should it take more than one), one write for the batch where each frame was a write of its own (a
/// system call on the shard each). Each frame goes as its payload's own bytes, read
/// where it is (this viewer's copy, moved: no other copy is made); one its catch-up held (at or under `have`) is
/// skipped, and `have` moves to the newest sent. A frame's delivery and wait, from its publish to the read
/// (`read.now_us`), are observed as it's fed; its time from the read (`read.at`) to its write once the batch is
/// written (goal 1, plan 01a1a59c2a68 step 2); and, to the microsecond, its wait from its publish for its viewer's task
/// to take it up (`read.woke_us`) and that write (goal 10, plan 01a1a87b2dae step 5: `crate::route_latency::FrameLeg`).
/// Written in its viewer's turn on its shard, `turns`, which the batches before it in the same poll took of (goal 9, plan
/// 01a1a87b2cec step 3).
async fn write_live_batch<S>(tx: &mut S, messages: Vec<kiln_seams::bus::Message>, have: &mut u64, patches: crate::harness::registry::Patches, topic: &str, read: Read, turns: &mut crate::rt::Turns) -> Result<Fed, S::Error>
where
    S: futures::Sink<ws::Message> + Unpin,
{
    use futures::SinkExt as _;
    let mut fed = 0u64;
    let mut came_to = Fed::Written;
    // A batch's frames are megabytes at the most (16 of the bus's largest messages): what's fed goes out, and its
    // turn is given up, once its next frame would pass the shard's quantum (goal 10, plan 01a1a87b2dae step 5: a
    // viewer's live polls held its shard 0.78 ms on the CPU at the longest in rt9's calm rounds).
    for m in messages {
        let Some(frame) = crate::harness::live::decode_ref(&m.payload) else {
            tracing::warn!(target: "kiln.bus", event = "feed.unreadable", topic = %topic, offset = m.offset, "a frame the feed didn't make: the viewer starts over");
            came_to = Fed::StartOver;
            break;
        };
        if frame.text == crate::harness::live::RESYNC {
            came_to = Fed::StartOver;
            break;
        }
        // The session's last frame: the viewer ends with it, and its place, given back, lets the topic go.
        if frame.text == crate::harness::live::closed() {
            tx.feed(ws::text(frame.for_viewer(patches))).await?;
            came_to = Fed::Closed;
            break;
        }
        // One its catch-up already held.
        if frame.rev != 0 && frame.rev <= *have {
            continue;
        }
        let (rev, range) = (frame.rev, frame.range_for_viewer(patches));
        let published = crate::harness::live::published_us(&m.payload);
        let said = crate::harness::live::into_text(m.payload, range);
        // A state or changeset that went as its patch alone, to a viewer that takes it whole (none did as it
        // went: `live::Feed::wants_whole`): it starts over rather than take nothing.
        if said.is_empty() {
            came_to = Fed::StartOver;
            break;
        }
        if let Some(at) = published {
            let waited = Duration::from_micros(read.now_us.saturating_sub(at));
            crate::metrics::bus().delivery_seconds.observe(waited.as_secs_f64());
            crate::route_latency::observe_wait(crate::route_latency::Class::Websocket, waited);
            crate::route_latency::observe_frame(read.woken, Duration::from_micros(read.woke_us.saturating_sub(at)));
        }
        *have = (*have).max(rev);
        tx.feed(ws::text(said)).await?;
        fed = fed.saturating_add(1);
        if turns.over() {
            tx.flush().await?;
            hyper_rt::futures::yield_now().await;
            *turns = crate::rt::Turns::new();
        }
    }
    if fed > 0 || came_to == Fed::Closed {
        tx.flush().await?;
    }
    let wrote = read.at.elapsed();
    crate::route_latency::observe_took(crate::route_latency::Class::Websocket, wrote, fed);
    crate::route_latency::observe_frames(crate::route_latency::FrameLeg::Write, wrote, fed);
    Ok(came_to)
}

/// The session sockets' viewers that ran on another shard than their session's owner, and on it, in this test process
/// (goal 10, plan 01a1a87b2dae step 5): each viewer is served on its owner's shard (`harness_ws`).
#[cfg(test)]
static VIEWERS_ON_OWNER: [std::sync::atomic::AtomicUsize; 2] = [std::sync::atomic::AtomicUsize::new(0), std::sync::atomic::AtomicUsize::new(0)];

/// What a viewer's socket is doing, for the shards' table of what held them (`hyper_rt::slow`; goal 10, plan
/// 01a1a87b2dae step 5: g9p7's calm lab named the socket's task by its spawn site alone, held 6.3 ms, and not which
/// of its parts held it): its attach (what's kept of its files, its place among the viewers, the view), its
/// snapshot's pieces, a catch-up after it fell behind, its live frames, a command it sent.
static VIEWER_ATTACH: hyper_rt::slow::Label = hyper_rt::slow::Label { name: "viewer attach" };
static VIEWER_SNAPSHOT: hyper_rt::slow::Label = hyper_rt::slow::Label { name: "viewer snapshot" };
static VIEWER_CATCH_UP: hyper_rt::slow::Label = hyper_rt::slow::Label { name: "viewer catch-up" };
static VIEWER_LIVE: hyper_rt::slow::Label = hyper_rt::slow::Label { name: "viewer live" };
static VIEWER_COMMAND: hyper_rt::slow::Label = hyper_rt::slow::Label { name: "viewer command" };

/// A viewer of one agent session: the snapshot, then live updates (JSON frames tagged `t`); the
/// viewer sends commands (JSON tagged `type`). `{"t": "synced"}` follows each snapshot sent here
/// (on attach, and again after falling behind): the history resent since its `open` is complete,
/// so a viewer can swap it in at once instead of clearing what it shows.
///
/// The live updates come from the session's feed on kiln's bus (docs/bus.md), read from the
/// offset the snapshot was taken at, so none is missed or seen twice. A viewer that falls behind
/// what the feed keeps is refused `Behind` (the bus's lag notice) and starts over from a snapshot,
/// as it does at the feed's resync marker; nothing it does holds up the session or another viewer.
async fn harness_attach(hub: Arc<Hub>, id: String, socket: ws::WebSocket, takes: Takes, scope: String, acts: bool, who: String) -> anyhow::Result<()> {
    use futures::FutureExt as _;
    use kiln_seams::Bus as _;
    use kiln_seams::bus::Refusal;
    let (mut tx, rx) = socket.split();
    // What its person sends, read only once its socket has some (goal 9, plan 01a1a87b2cec step 5): every frame
    // and answer wakes this loop, and an ask with nothing come zero-fills the reader's 16 KiB and reads nothing.
    let mut rx = ws::Gated::new(rx);
    // What it can't read of what its person sends, and what they can't read of what it sends,
    // shows on the agent's page.
    let sock = ui::Socket::of(Wire::SessionUp, Wire::SessionDown, &who, Some(Page::Session(id.clone())));
    // What's kept of its files read and its frame made off the shards (`crate::harness::kept::warm`): the attach
    // copies that frame.
    hyper_rt::futures::label(&VIEWER_ATTACH);
    crate::harness::kept::warm(&hub, &id).await;
    hyper_rt::futures::label(&VIEWER_ATTACH);
    let Some(att) = hub.harness.attach_from(&id, takes.after, takes.patches, takes.view) else {
        tx.send(ws::text(SessionDown::from(SessionFrame::Closed).text())).await?;
        return Ok(());
    };
    // The attach's poll ends here, and its snapshot begins in a poll of its own (goal 10, plan 01a1a87b2dae step 5):
    // the shards' table counts a poll against the last label it set, so an attach's work and the start of its
    // snapshot in one poll were counted as the snapshot's, and held the shard for the two together.
    hyper_rt::futures::yield_now().await;
    {
        // Counted while it's written (goal 10, plan 01a1a87b2dae step 3's trace).
        let _writing = crate::reads::Writing::of(&hub.harness.views.attaching);
        crate::rt::labeled(&VIEWER_SNAPSHOT, send_snapshot(&mut tx, att.snapshot, takes.packed)).await?;
    }
    // Its snapshot's poll ends with its last frame, and what was published meanwhile goes in polls of its own (goal 9,
    // plan 01a1a87b2cec step 3): read at once, those frames went on in the poll the snapshot's last turn took, a turn of
    // their own after it.
    hyper_rt::futures::yield_now().await;
    // Its place among the session's viewers, given back as it goes.
    let viewer = att.viewer;
    let topic = att.topic;
    let mut at = att.at;
    // The feed's refusals as of the snapshot it has.
    let mut refusals = att.refusals;
    // Commands sent, their answers still to come (the frame each comes to, if any): waited for
    // beside everything else, so what's live and what the viewer sends next (a stop) never waits
    // for them.
    let mut answers = futures::stream::FuturesUnordered::<futures::future::BoxFuture<'static, Option<String>>>::new();
    // The newest revision of the copy this viewer has: what it catches up from when it starts
    // over, and what a live frame it already has (a catch-up held it) is told by (docs/wire.md,
    // "Revisions").
    let mut have = att.rev;
    let closed = || ws::text(SessionDown::from(SessionFrame::Closed).text());
    // The writer of its commands' audit rows (`audits`), from its first command.
    let mut audit: Option<crate::memory::queue::Sender<String>> = None;
    // Its last catch-up: when it ended, and how long it took to make and send.
    let mut caught_up: Option<(Instant, Duration)> = None;
    // Its turn on its shard (goal 9, plan 01a1a87b2cec step 3): the batches of its feed read at once one after another
    // (a viewer behind its feed) are written in one turn, its poll ending once the next frame would pass the shard's
    // quantum (`write_live_batch`, `crate::rt::Turns::end`). Each batch began a turn of its own and gave its shard up
    // only where something else wanted it (`crate::rt::consume_budget`), so a poll of several went on past the quantum.
    // A batch the loop waited for begins a turn afresh: the wait gave its shard to its other work.
    let mut turns = crate::rt::Turns::new();
    // Whether it runs on its session's owner's shard, where its frames are published: then their wake is no other
    // thread's (goal 10, plan 01a1a87b2dae step 5).
    let own = crate::rt::shard_index() == Some(hub.harness.owner_shard(&id));
    #[cfg(test)]
    if let Some(n) = VIEWERS_ON_OWNER.get(usize::from(own)) {
        n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    loop {
        let live = {
            let (bus, topic, from) = (&hub.bus, &topic, at);
            waited(async move {
                match from {
                    Some(from) => {
                        let wait = bus.wait(topic, from).fuse();
                        let quiet = crate::rt::sleep(FEEDLESS).fuse();
                        futures::pin_mut!(wait, quiet);
                        futures::select_biased! {
                            w = wait => match w {
                                Ok(()) => Live::Ready,
                                Err(e) => Live::Refused(e),
                            },
                            () = quiet => Live::Quiet,
                        }
                    }
                    None => {
                        crate::rt::sleep(FEEDLESS).await;
                        Live::Feedless
                    }
                }
            })
        };
        // Set when what the feed holds for this viewer isn't what it should take next.
        let mut start_over = false;
        futures::select_biased! {
            frame = answers.select_next_some() => {
                if let Some(frame) = frame {
                    tx.send(ws::text(frame)).await?;
                }
            }
            (w, fresh) = live.fuse() => match w {
                // This task's poll took its feed's word now: each frame's wait for it is the viewer's wake (goal 10, plan
                // 01a1a87b2dae step 5).
                Live::Ready => match (crate::harness::live::now_us(), crate::rt::labeled(&VIEWER_LIVE, hub.bus.read(&topic, at.unwrap_or_default(), FEED_READ)).await) {
                    (woke, Ok(batch)) => {
                        if fresh {
                            turns = crate::rt::Turns::new();
                        }
                        // Read now: each frame's delivery, from its publish on this process's
                        // microsecond clock (the bus's own stamps are milliseconds).
                        let (read, next) = (Read::now(woke, own), batch.next);
                        match crate::rt::labeled(&VIEWER_LIVE, write_live_batch(&mut tx, batch.messages, &mut have, takes.patches, &topic, read, &mut turns)).await? {
                            Fed::Written => {}
                            Fed::StartOver => start_over = true,
                            Fed::Closed => return Ok(()),
                        }
                        at = Some(next);
                        // A viewer behind its feed reads batch after batch, each ready at once: its turn ends once its
                        // next would pass the quantum, whether or not anything else wants the shard then.
                        turns.end().await;
                    }
                    (_, Err(Refusal::Behind { .. })) => {
                        crate::metrics::bus().lag_notices.inc();
                        start_over = true;
                    }
                    (_, Err(Refusal::NoTopic(_))) => {
                        tx.send(closed()).await?;
                        return Ok(());
                    }
                    (_, Err(e)) => {
                        tracing::warn!(target: "kiln.bus", event = "feed.refused", topic = %topic, error = %e, "a viewer's read of its session's feed was refused: it starts over");
                        start_over = true;
                    }
                },
                Live::Refused(Refusal::NoTopic(_)) => {
                    tx.send(closed()).await?;
                    return Ok(());
                }
                Live::Refused(e) => {
                    tracing::warn!(target: "kiln.bus", event = "feed.refused", topic = %topic, error = %e, "a viewer's wait on its session's feed was refused: it starts over");
                    start_over = true;
                }
                Live::Feedless => start_over = true,
                Live::Quiet => start_over = viewer.refusals() != refusals,
            },
            m = rx.next().fuse() => match ui::came(m) {
                Came::Text(t) => match ui::session_up(t.as_str()) {
                    // A frame that came down that the page couldn't read: the server, which sent it,
                    // notes it.
                    Ok(SessionUp::Unread(f)) => hub.note_told(&sock, &f).await,
                    // Watching isn't commanding: refused, under its id when it came under one.
                    Ok(SessionUp::Under { id: cmd, .. }) if !acts => tx.send(ws::text(crate::harness::asked::nack(Some(&cmd), "refused", WATCHING))).await?,
                    Ok(SessionUp::Bare(_)) if !acts => tx.send(ws::text(session_error(WATCHING.into()))).await?,
                    // Sent now, in the order the viewer sent them; each answer comes later. Under
                    // its client's id (docs/wire.md, "The UI's sockets"): taken once however often
                    // it comes, and answered on this socket once, `ack` or `nack`.
                    Ok(SessionUp::Under { id: cmd, c, .. }) => {
                        use crate::harness::asked::{Taken, frame};
                        hyper_rt::futures::label(&VIEWER_COMMAND);
                        let kind = hub.harness.kind_of(&hub, &id, &c);
                        match hub.harness.asked.take(&id, &scope, &cmd, kind, Instant::now()) {
                            (kind, Taken::New(rx)) => {
                                audit_command(&hub, &mut audit, &who, &id, &c).await;
                                hub.harness.send_asked(&hub, &id, &scope, &cmd, c).await;
                                answers.push(answer_of(cmd, rx, kind));
                            }
                            (kind, Taken::Waiting(rx)) => answers.push(answer_of(cmd, rx, kind)),
                            (kind, Taken::Answered(sent)) => tx.send(ws::text(frame(&cmd, &sent, kind))).await?,
                        }
                    }
                    // As clients from before ids send them: refusals said as errors, nothing for
                    // the rest.
                    Ok(SessionUp::Bare(c)) => {
                        hyper_rt::futures::label(&VIEWER_COMMAND);
                        audit_command(&hub, &mut audit, &who, &id, &c).await;
                        let pending = hub.harness.dispatch(&hub, &id, c.clone()).await;
                        let (h, session) = (Arc::clone(&hub), id.clone());
                        answers.push(async move { refusal_frame(h.harness.resolve(&h, &session, c, pending).await) }.boxed());
                    }
                    // What couldn't be read: noted, and answered `nack`, `unsupported` or
                    // `malformed`; told as clients from before ids are, too, when it came as theirs do.
                    Err(u) => {
                        hub.note_refused(&sock, &u.refused).await;
                        if u.bare {
                            tx.send(ws::text(session_error(format!("bad command: {}", u.refused.why)))).await?;
                        }
                        tx.send(ws::text(crate::harness::asked::unreadable(u.id.as_deref(), u.refused.code, &u.refused.why))).await?;
                    }
                },
                Came::Binary(_) => {
                    let r = ui::Refused::binary();
                    hub.note_refused(&sock, &r).await;
                    tx.send(ws::text(crate::harness::asked::unreadable(None, r.code, &r.why))).await?;
                }
                Came::Control => {}
                Came::Gone => return Ok(()),
                // Past its limit (or not a WebSocket frame): noted, and closed as RFC 6455 says.
                Came::Ends { close, refused } => {
                    hub.note_refused(&sock, &refused).await;
                    let _ = tx.close(Some(close)).await;
                    return Ok(());
                }
            },
        }
        if start_over {
            // One that has to start over again (its socket slower than the session goes, or the
            // feed refusing frame after frame) waits first until as long has gone by since its last
            // catch-up as that one took: it spends at most half its time catching up, and a
            // catch-up a frame can't cost the server a snapshot each, at whatever pace they cost.
            if let Some((ended, took)) = caught_up {
                let left = took.saturating_sub(ended.elapsed());
                if !left.is_zero() {
                    crate::rt::sleep(left).await;
                }
            }
            let began = Instant::now();
            // What it missed since what it has (it begins with "resumed"), or all of it when the copy
            // can't tell (it begins with "open"); and the feed from where that is. What's kept of its
            // files made off the shards first, as for an attach.
            hyper_rt::futures::label(&VIEWER_CATCH_UP);
            crate::harness::kept::warm(&hub, &id).await;
            hyper_rt::futures::label(&VIEWER_CATCH_UP);
            let Some((snapshot, from, now, rev)) = hub.harness.frames_since(&id, &viewer, have, takes.view) else {
                tx.send(closed()).await?;
                return Ok(());
            };
            // As for an attach: what was found and the catch-up's first piece in polls of their own.
            hyper_rt::futures::yield_now().await;
            {
                let _writing = crate::reads::Writing::of(&hub.harness.views.attaching);
                crate::rt::labeled(&VIEWER_CATCH_UP, send_snapshot(&mut tx, snapshot, takes.packed)).await?;
            }
            caught_up = Some((Instant::now(), began.elapsed()));
            at = from;
            refusals = now;
            have = have.max(rev);
            // As after an attach's snapshot: what was published meanwhile in polls of their own, a turn afresh.
            hyper_rt::futures::yield_now().await;
            turns = crate::rt::Turns::new();
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
    let (connected, agents) = hub.agents.load().get(machine).map_or((false, false), |a| (a.connected, a.terminal && !a.ephemeral));
    if !connected {
        return Err(err(StatusCode::CONFLICT, format!("{} is not connected", hub.shown(machine))));
    }
    if !agents {
        return Err(err(StatusCode::FORBIDDEN, format!("{} has terminals (and so agents) turned off", hub.shown(machine))));
    }
    match hub.dir_list(machine, &path).await {
        Ok(Some(listing)) => Ok(srv::json(StatusCode::OK, &listing)),
        Ok(None) => Err(err(StatusCode::CONFLICT, format!("{}'s runner is too old to list folders: upgrade it", hub.shown(machine)))),
        Err(e) => Err(err(StatusCode::BAD_GATEWAY, format!("{e:#}"))),
    }
}

// ------------------------------------------------------------------ misc

async fn metrics_handler(hub: &Hub, headers: &HeaderMap) -> Response {
    if !hub.cfg.server.metrics_public && !authed(hub, headers) {
        return err(StatusCode::UNAUTHORIZED, "authentication required").into_response();
    }
    // The bus's figures as its node states them now.
    hub.bus.observe();
    // Written out on the blocking pool, not on the request's shard: every family's every series, hundreds of
    // kilobytes, in one call, held a shard 2.5 ms in g9p7's calm lab (goal 10, plan 01a1a87b2dae step 5). What it
    // reads is the process's, not a shard's.
    let text = match crate::rt::blocking(crate::metrics::render).await {
        Ok(text) => text,
        Err(e) => return err(StatusCode::SERVICE_UNAVAILABLE, format!("the figures couldn't be written out: {e:#}")).into_response(),
    };
    srv::response(StatusCode::OK, "text/plain; version=0.0.4", srv::full(text))
}

async fn readyz(hub: &Hub) -> Response {
    // Overload (goal 6, step 4): not ready while every shard sheds the API; else ready, saying what is shed.
    let overload = crate::overload::readiness();
    if let Some((true, why)) = &overload {
        return srv::text(StatusCode::SERVICE_UNAVAILABLE, why);
    }
    let note = overload.map(|(_, why)| format!(" ({why})")).unwrap_or_default();
    // Observe's store, open but replaying its log after a start, answers part of what it holds:
    // not ready until it has applied it all (seconds). A store that didn't open doesn't hold the
    // server back: the rest of kiln answers without it.
    if let Some(o) = crate::observe::api::get()
        && o.store().is_some()
        && !o.replayed()
    {
        let v = o.view();
        return srv::text(StatusCode::SERVICE_UNAVAILABLE, &format!("observe's store is replaying its log ({} of {} entries applied)", v.applied, v.replay_to));
    }
    // The store, from what the server knows of it (goal 6, plan 01a1a59c2abe step 3): readiness is Critical, and the
    // `SELECT 1` it sent through kiln.db's one writer waited behind every write before it (the lab's writer line, 0.7 s
    // at p99; a deploy rolled back on it, 2026-10-30). Ready while its writer runs and keeps up; not once its thread has
    // stopped, or while calls wait and it has finished none for as long as kiln's longest prober waits
    // (`crate::pool_health::WRITER_STUCK`), so no prober sees it unready where the call would have answered in time.
    if !hub.store.writer_running() {
        return srv::text(StatusCode::SERVICE_UNAVAILABLE, "store: its writer has stopped");
    }
    match crate::pool_health::store_writer_stuck() {
        Some((waiting, still)) => srv::text(StatusCode::SERVICE_UNAVAILABLE, &format!("store: {waiting} calls wait for its writer, which has finished none for {} s", still.as_secs())),
        None => srv::text(StatusCode::OK, &format!("ready{note}")),
    }
}

/// `/healthz/delay`: how late this server's shards run ready work, from the shards' own figures
/// (`crate::shard_health`; no shard is asked), the target headroom probes on a runner beside it
/// (docs/priority.md; goal 8, plan 01a1a59c2b29 step 5). JSON, every time in seconds:
/// - `delay_s`: the worst shard's now ([`worst_delay`]), for a person to read;
/// - `late_s` and `probes`: every probe's lateness added up, and how many probes ran, all shards
///   together and since the server started, so a reader's two reads say how late ready work ran on
///   average between them, every probe counted;
/// - `held_s`: how long past its period a shard held now has gone without a probe (a hold shows
///   there before any probe can say how late it ran; a parked shard isn't held: its probe waits up to
///   a period more for a wake, `crate::shard_health::Snapshot::held`);
/// - `period_s`: the probes' period.
///
/// Open, as `/healthz` is; 503 where the shards haven't been probed yet.
fn shard_delay() -> Response {
    let shards = crate::shard_health::snapshots();
    match delay_answer(&shards, crate::shard_health::PERIOD) {
        Some(v) => srv::json(StatusCode::OK, &v),
        None => srv::text(StatusCode::SERVICE_UNAVAILABLE, "this server's shards haven't been probed yet"),
    }
}

/// [`shard_delay`]'s answer from the shards' figures, once any shard has been probed.
fn delay_answer(shards: &[crate::shard_health::Snapshot], period: Duration) -> Option<serde_json::Value> {
    let worst = worst_delay(shards.iter().copied(), period)?;
    let late: Duration = shards.iter().map(|s| s.late_sum).sum();
    let probes: u64 = shards.iter().map(|s| s.probes).sum();
    let held = shards.iter().map(|s| s.held(period)).max().unwrap_or_default();
    Some(json!({ "delay_s": worst.as_secs_f64(), "late_s": late.as_secs_f64(), "probes": probes, "held_s": held.as_secs_f64(), "period_s": period.as_secs_f64() }))
}

/// The worst of the shards' delay now: each one's latest probe, how late it ran, or, for a shard held
/// now, how long past its `period` it has gone without one (a hold shows there before its probe can
/// say how late it ran; a parked shard isn't held, `crate::shard_health::Snapshot::held`).
fn worst_delay(shards: impl Iterator<Item = crate::shard_health::Snapshot>, period: Duration) -> Option<Duration> {
    shards.filter(|s| s.probes > 0 || s.since_probe.is_some()).map(|s| s.late.max(s.held(period))).max()
}

/// `/healthz/server`: how this server is, for the checks that watch a deploy of kiln
/// (docs/cd-targets.md §10). The commit it runs; how long it has been up; how many times kiln's
/// supervisor started it again (a crash, or the kernel's OOM killer: neither shows in its pod's
/// status); its container's memory, from its cgroup; and its runners, connected now and just
/// before it started, and the share of those back (checks' criteria compare, they don't count); and
/// how it runs on its machine (its shards' real-time class, where its own runner's legs and agents
/// run). Numbers, the commit and how it runs only, nothing anyone's, so it's open, as `/healthz` is.
fn server_health(hub: &Hub) -> Response {
    let memory = crate::os::cgroup::memory().map(|m| json!({ "used": m.used, "limit": m.limit, "share": m.share(), "peak": m.peak, "oom_kills": m.oom_kills }));
    let connected = hub.agents.load().values().filter(|a| a.connected && !a.ephemeral).count() as u64;
    let before = hub.runners_before.load(std::sync::atomic::Ordering::Relaxed);
    srv::json(
        StatusCode::OK,
        &json!({
            "commit": COMMIT,
            "version": env!("CARGO_PKG_VERSION"),
            "uptime_s": hub.started.elapsed().as_secs(),
            "restarts": hub.generation.saturating_sub(1),
            "memory": memory,
            // What this build reads, and what its data needs (docs/resilience.md, G2): a guard
            // doesn't roll back to a build older than `min_reader`.
            "schema": { "epoch": crate::store::migrations::SCHEMA_EPOCH, "min_reader": crate::store::migrations::min_reader() },
            "runners": { "connected": connected, "before": before, "back": back(connected, before) },
            // Its place on its machine (docs/priority.md; goal 8, plan 01a1a59c2b29 step 7): its shards'
            // real-time class as they took it, or why not (`[server] realtime`), their nudge in its place
            // (`[server] nudge`), and where the legs and agents its own runner starts run beside its cores
            // (`[server] cores`); its cores, its shards' and the partition they're in, if any; the spin before each
            // park its shards were asked for (`KILN_RT_SPIN`, the lab's; goal 9, plan 01a1a87b2cec step 4).
            "priority": { "realtime": crate::rt::realtime_status(), "nudge": crate::rt::nudge_status(), "work": crate::os::work_said(), "cores": crate::rt::cores_status(), "spin": crate::rt::spin_status() },
            // The cluster group (docs/p2p.md P1, P2): kiln.db's last write, the log's last entry,
            // the runners holding a replica, and kiln.db's size (what a runner's copy takes).
            "store": hub.store.group_standing().map_or(json!({ "group": false }), |(seq, applied)| json!({
                "group": true,
                "seq": seq,
                "applied": applied,
                "learners": hub.store.group().map_or(0, |g| g.point().1.learners.len()),
                "bytes": hub.store.file_bytes(),
            })),
        }),
    )
}

/// The share of the runners connected before a start that are connected now, to three places:
/// all of them when there were none.
fn back(connected: u64, before: u64) -> f64 {
    if before == 0 {
        return 1.0;
    }
    (connected.min(before) as f64 / before as f64 * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A socket's sending half as a test sees it: what was sent it, and how often it was flushed. One `slow` takes
    /// each frame in its shard's whole quantum, as read while it takes it (the shard's own cell, which nothing changes
    /// during a poll), so the turn that fed it is over (goal 10, plan 01a1a87b2dae step 5).
    #[derive(Default)]
    struct Written {
        sent: Vec<ws::Message>,
        flushes: usize,
        /// How many had been sent at each flush.
        flushed_at: Vec<usize>,
        slow: bool,
    }

    impl futures::Sink<ws::Message> for Written {
        type Error = std::io::Error;
        fn poll_ready(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn start_send(self: std::pin::Pin<&mut Self>, item: ws::Message) -> Result<(), Self::Error> {
            if self.slow
                && let Some(quantum) = hyper_rt::futures::step_budget_ns()
            {
                std::thread::sleep(Duration::from_nanos(quantum));
            }
            self.get_mut().sent.push(item);
            Ok(())
        }
        fn poll_flush(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), Self::Error>> {
            let this = self.get_mut();
            this.flushes += 1;
            this.flushed_at.push(this.sent.len());
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_close(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// Goal 4, plan 01a1a59c2a9c step 5 (as goal 9 step 5 found), on the feed the sockets read since
    /// W21-R15: an events socket sends every event that's ready, the hub's and the sessions' own, written
    /// as one (one flush, not one an event); a feed that let some go says `Resync`, in one write; and two
    /// sockets sending the same event send one allocation, the event's own text, not a copy each.
    #[test]
    fn g4_s5_an_events_socket_writes_whats_ready_at_once_its_bytes_shared() -> anyhow::Result<()> {
        use crate::wire::events::{ServerEvent, kind_of};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let from = hub.feed.head();
            let info = crate::harness::session::Info { id: "s1".into(), kind: "claude".into(), ..Default::default() };
            hub.emit(&ServerEvent::Updates);
            // Its text held, as its copy holds it until its next info: the sockets read it.
            let _info = hub.emit_harness("s1", &serde_json::to_vec(&info)?);
            hub.emit(&ServerEvent::Terminals);
            hub.emit(&ServerEvent::HarnessClosed { id: "s1".into() });
            let (mut one, mut two) = (Written::default(), Written::default());
            for w in [&mut one, &mut two] {
                let mut out = crate::wire::events::feed::Outbox::new(None);
                let at = take_since(&hub, w, from, &mut out, false, true, &mut None, &mut None).await?;
                anyhow::ensure!(at >= from + 4, "it read on past ours: {at} from {from}");
            }
            let text = |m: &ws::Message| match m {
                ws::Message::Text(t) => Some(t.as_str().to_owned()),
                _ => None,
            };
            let kinds: Vec<String> = one.sent.iter().filter_map(text).filter_map(|t| kind_of(&t).map(str::to_owned)).collect();
            // The hub's own work may emit beside these: each of ours once, the hub's before the sessions'.
            for k in ["updates", "terminals", "harness", "harness_closed"] {
                anyhow::ensure!(kinds.iter().filter(|s| *s == k).count() == 1, "{k} in {kinds:?}");
            }
            anyhow::ensure!(one.flushes == 1 && two.flushes == 1, "flushed {} and {} times for {} events", one.flushes, two.flushes, one.sent.len());
            // Each of ours went to both sockets as one allocation between them.
            let ours = |w: &Written| -> Vec<(String, usize)> {
                w.sent
                    .iter()
                    .filter_map(|m| match m {
                        ws::Message::Text(t) if kind_of(t.as_str()).is_some_and(|k| ["updates", "terminals", "harness", "harness_closed"].contains(&k)) => Some((t.as_str().to_owned(), t.as_str().as_ptr().addr())),
                        _ => None,
                    })
                    .collect()
            };
            let (a, b) = (ours(&one), ours(&two));
            anyhow::ensure!(a.len() == 4 && a == b, "not the same bytes: {a:?} and {b:?}");
            // A feed that let some go past where a socket was: `Resync`, in one write.
            let behind = hub.feed.head();
            for k in 0..=crate::wire::events::feed::RING {
                hub.feed.push(Arc::from(format!(r#"{{"type":"updates","n":{k}}}"#)));
            }
            let mut three = Written::default();
            let mut out = crate::wire::events::feed::Outbox::new(None);
            let at = take_since(&hub, &mut three, behind, &mut out, false, true, &mut None, &mut None).await?;
            let said: Vec<String> = three.sent.iter().filter_map(text).collect();
            // It starts over from where the feed is: `Resync`, and nothing from before the gap.
            anyhow::ensure!(said == [ServerEvent::Resync.text()] && three.flushes == 1, "{said:?}");
            let after = r#"{"type":"updates","n":"after"}"#;
            hub.feed.push(Arc::from(after));
            let mut four = Written::default();
            take_since(&hub, &mut four, at, &mut out, false, true, &mut None, &mut None).await?;
            let said: Vec<String> = four.sent.iter().filter_map(text).collect();
            anyhow::ensure!(said.iter().any(|s| s == after) && !said.iter().any(|s| s.starts_with(r#"{"type":"updates","n":"#) && s != after), "then what comes after it: {said:?}");
            Ok(())
        })
    }

    /// The text frame of a shared text is that text's own bytes, not a copy.
    #[test]
    fn g4_s5_a_shared_text_frame_is_its_texts_own_bytes() -> anyhow::Result<()> {
        let text: Arc<str> = Arc::from(r#"{"type":"harness","info":{"id":"s1","summary":"a \"quoted\" ünïcode word"}}"#);
        for m in [ws::shared_text(&text), ws::shared_text(&text)] {
            let ws::Message::Text(t) = &m else { anyhow::bail!("not text") };
            anyhow::ensure!(t.as_str() == &*text && std::ptr::eq(t.as_str().as_ptr(), text.as_ptr()), "copied");
        }
        Ok(())
    }

    /// Goal 8 (plan 01a1a59c2b29 step 5, docs/priority.md): the server's delay is its worst shard's,
    /// the latest probe's lateness, or a hold under way past the probes' period; a shard not probed
    /// yet says nothing; a parked one isn't held however long since its probe ran (goal 9, plan
    /// 01a1a87b2cec step 5: its quiet probe waits up to a period more for a wake).
    #[test]
    fn g8_s5_the_delay_is_the_worst_shards_now() {
        let period = Duration::from_millis(10);
        let shard = |probes: u64, late_us: u64, since_ms: Option<u64>| crate::shard_health::Snapshot {
            probes,
            late_sum: Duration::from_micros(late_us * probes),
            late: Duration::from_micros(late_us),
            most_late: Duration::from_micros(late_us),
            since_probe: since_ms.map(Duration::from_millis),
            busy: None,
            polls: 0,
            slow: crate::shard_health::Slow::default(),
            longest_poll: Duration::ZERO,
            overrun: Duration::ZERO,
            tasks: 0,
            most_tasks: 0,
            parked: false,
        };
        assert_eq!(worst_delay([shard(9, 80, Some(4)), shard(9, 2_000, Some(7))].into_iter(), period), Some(Duration::from_micros(2_000)));
        // Parked 18 ms since its probe: idle, not held.
        let parked = crate::shard_health::Snapshot { parked: true, ..shard(9, 80, Some(18)) };
        assert_eq!(worst_delay([shard(9, 80, Some(4)), parked].into_iter(), period), Some(Duration::from_micros(80)));
        assert_eq!(parked.held(period), Duration::ZERO);
        // Held now: 1.5 s since its probe, past its 10 ms period.
        assert_eq!(worst_delay([shard(9, 80, Some(4)), shard(9, 100, Some(1_500))].into_iter(), period), Some(Duration::from_millis(1_490)));
        assert_eq!(worst_delay([shard(0, 0, None)].into_iter(), period), None);
        // The answer: that, and what a reader works out how late ready work ran from between two reads
        // (every probe's lateness added up, and how many), the hold, and the period.
        let v = delay_answer(&[shard(9, 80, Some(4)), shard(3, 2_000, Some(1_500))], period).unwrap_or_default();
        let s = |k: &str| v.get(k).and_then(serde_json::Value::as_f64).unwrap_or(-1.0);
        assert!((s("delay_s") - 1.49).abs() < 1e-9, "{v}");
        assert!((s("late_s") - (9.0 * 80e-6 + 3.0 * 2e-3)).abs() < 1e-9, "{v}");
        assert_eq!(v.get("probes").and_then(serde_json::Value::as_u64), Some(12), "{v}");
        assert!((s("held_s") - 1.49).abs() < 1e-9 && (s("period_s") - 0.01).abs() < 1e-12, "{v}");
        assert_eq!(delay_answer(&[shard(0, 0, None)], period), None);
    }

    /// P2: a learner's copy streams whole though the next copy, made once this one is handed over,
    /// clears its name (what a stop left behind) before this one's reader has run.
    #[test]
    fn a_learner_s_copy_streams_whole_though_its_name_goes_once_handed_over() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("copy.db");
            let want: Vec<u8> = (0..(3_u32 << 18)).map(|i| u8::try_from(i % 251).unwrap_or(0)).collect();
            std::fs::write(&path, &want)?;
            let (size, rx) = copy_body(path.clone())?;
            // The next copy's sweep, before this one's reader has run (Windows refuses: it's open).
            std::fs::remove_file(&path).ok();
            let mut got = Vec::new();
            while let Ok(chunk) = rx.recv().await {
                got.extend_from_slice(&chunk);
            }
            anyhow::ensure!(u64::try_from(want.len())? == size && got == want, "streamed {} of {} bytes", got.len(), want.len());
            Ok(())
        })
    }

    /// Goal 4, plan 01a1a59c2a9c step 5: a response's pieces read ahead of their connection, on a lock-free
    /// queue now. A learner's copy of kiln.db, read by a thread of its own, stops at [`COPY_AHEAD`] pieces
    /// while nothing takes them (its connection behind) and goes on as they're taken; its body gives every
    /// byte once, in order.
    #[test]
    fn g4_s5_a_copy_read_ahead_stops_at_its_places_and_streams_whole() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            use http_body_util::BodyExt as _;
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("copy.db");
            // Three times the pieces it may read ahead, of the 256 KiB it reads at a time.
            let want: Vec<u8> = (0..(3 * COPY_AHEAD * (256 << 10))).map(|i| u8::try_from(i % 251).unwrap_or(0)).collect();
            std::fs::write(&path, &want)?;
            let (size, rx) = copy_body(path)?;
            let deadline = Instant::now() + Duration::from_secs(10);
            while rx.len() < COPY_AHEAD && Instant::now() < deadline {
                crate::rt::sleep(Duration::from_millis(10)).await;
            }
            crate::rt::sleep(Duration::from_millis(100)).await;
            anyhow::ensure!(rx.len() == COPY_AHEAD, "{} pieces read ahead, of {COPY_AHEAD}", rx.len());
            let got = srv::channel_body(rx).collect().await?.to_bytes();
            anyhow::ensure!(u64::try_from(want.len())? == size && got.as_ref() == want.as_slice(), "streamed {} of {} bytes", got.len(), want.len());
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 3: a learner's copy is read, let go of and removed by its job on the blocking
    /// pool ([`copy_out`]), however its reading ends, where its task's last poll removed it on a shard (1.9 to 4.0 ms
    /// on the CPU in g9p16's calm rounds). Read whole: every byte in order, the body ended, its name gone once the job
    /// returns. Its reader gone first: its name gone all the same.
    #[test]
    fn g9_s3_a_learners_copy_is_read_and_removed_by_its_job() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        // Three of the pieces it reads at a time, fewer than the places it reads ahead into: no wait for a reader.
        let want: Vec<u8> = (0..(3_u32 << 18)).map(|i| u8::try_from(i % 251).unwrap_or(0)).collect();
        let path = dir.path().join("whole.db");
        std::fs::write(&path, &want)?;
        let (tx, rx) = read_ahead(COPY_AHEAD);
        copy_out(std::fs::File::open(&path)?, tx, &path)?;
        anyhow::ensure!(!path.exists(), "the copy's name is still there once its job returned");
        let mut got = Vec::new();
        let ended = loop {
            match rx.try_recv() {
                Ok(piece) => got.extend_from_slice(&piece),
                Err(e) => break matches!(e, async_channel::TryRecvError::Closed),
            }
        };
        anyhow::ensure!(ended && got == want, "read {} of {} bytes, the body ended: {ended}", got.len(), want.len());
        let path = dir.path().join("unread.db");
        std::fs::write(&path, &want)?;
        let (tx, rx) = read_ahead(COPY_AHEAD);
        drop(rx);
        copy_out(std::fs::File::open(&path)?, tx, &path)?;
        anyhow::ensure!(!path.exists(), "the copy's name is still there once its reader went");
        Ok(())
    }

    /// Goal 4, plan 01a1a59c2a9c step 5: a socket's audit rows, on a lock-free queue now: with the store held,
    /// its commands go on until [`AUDITS_WAITING`] rows wait (and the one being written), then wait for room
    /// (a row is never dropped); once the store is free every row is written, in the order the commands came.
    #[test]
    fn g4_s5_a_sockets_audit_rows_wait_for_room_and_keep_their_order() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            use std::sync::atomic::{AtomicUsize, Ordering};
            let hub = crate::hub::tests::scratch_hub().await?;
            // The store's one connection held until the test lets it go.
            let (held, holding) = crate::memory::queue::places::<()>(&crate::memory::owners::CHANNELS, 1);
            let (free, freed) = std::sync::mpsc::channel::<()>();
            let busy = hub.store.clone();
            let hold = crate::rt::spawn(async move {
                busy.call(move |_| {
                    let _ = held.send_blocking(());
                    let _ = freed.recv_timeout(Duration::from_secs(30));
                    Ok(())
                })
                .await
            });
            holding.recv().await?;
            let rows = AUDITS_WAITING + 8;
            let went = Arc::new(AtomicUsize::new(0));
            let commands = crate::rt::spawn({
                let (hub, went) = (Arc::clone(&hub), Arc::clone(&went));
                async move {
                    let mut audit = None;
                    for n in 0..rows {
                        let send: crate::harness::model::Command = serde_json::from_value(json!({"type": "send", "id": format!("m{n}"), "text": "go"}))?;
                        audit_command(&hub, &mut audit, "user:dev1", &format!("row-{n}"), &send).await;
                        went.store(n + 1, Ordering::Release);
                    }
                    anyhow::Ok(())
                }
            });
            crate::rt::sleep(Duration::from_millis(300)).await;
            let gone = went.load(Ordering::Acquire);
            anyhow::ensure!(gone <= AUDITS_WAITING + 1, "{gone} commands went with the store held: their rows didn't wait for room");
            let _ = free.send(());
            commands.await.map_err(|_| anyhow::anyhow!("the commands never went"))??;
            let mut q = HashMap::new();
            q.insert("limit".to_owned(), "1000".to_owned());
            let deadline = Instant::now() + Duration::from_secs(10);
            let written = loop {
                let all = crate::audit::list(&hub.store, &q).await?;
                let mut written: Vec<String> = all
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|r| r.get("action").and_then(serde_json::Value::as_str) == Some("agent.command"))
                    .map(|r| r.get("detail").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned())
                    .collect();
                // Newest first: in the order they were written.
                written.reverse();
                if written.len() >= rows || Instant::now() > deadline {
                    break written;
                }
                crate::rt::sleep(Duration::from_millis(50)).await;
            };
            let want: Vec<String> = (0..rows).map(|n| format!("row-{n}: send")).collect();
            anyhow::ensure!(written == want, "{} rows of {rows}: lost, doubled or out of order", written.len());
            let _ = hold.await;
            Ok(())
        })
    }

    #[test]
    fn the_servers_own_machine_asking_for_its_local_agents_name_is_not_given_another_runner() {
        let local = Some("adas-macbook-pro.local");
        // The 2026-10-30 case: the server's Mac joined its own server and was named `<name>-2`.
        assert!(own_machines_runner(local, "Adas-MacBook-Pro.local", "adas-macbook-pro.local", "Adas-MacBook-Pro.local"));
        // Another machine with that name, the same machine under another name, or no local agent.
        assert!(!own_machines_runner(local, "Adas-MacBook-Pro.local", "adas-macbook-pro.local", "fedora"));
        assert!(!own_machines_runner(local, "Adas-MacBook-Pro.local", "build-box", "Adas-MacBook-Pro.local"));
        assert!(!own_machines_runner(None, "Adas-MacBook-Pro.local", "adas-macbook-pro.local", "Adas-MacBook-Pro.local"));
    }

    #[test]
    fn a_viewer_takes_what_its_query_asks_for() {
        use crate::harness::registry::Patches;
        let t = Takes::of(Some("z=gzip&patch=state"));
        assert!(t.packed && t.patches == Patches { state: true, changes: false });
        let t = Takes::of(Some("patch=state"));
        assert!(!t.packed && t.patches.state);
        let t = Takes::of(None);
        assert!(!t.packed && t.patches == Patches::default());
        assert!(!Takes::of(Some("z=gzipped&patch=states")).packed);
        // A changeset as what changed in it, asked for on its own or with a state's.
        assert_eq!(Takes::of(Some("z=gzip&patch=state&patch=changes")).patches, Patches { state: true, changes: true });
        assert_eq!(Takes::of(Some("patch=changes")).patches, Patches { state: false, changes: true });
        // A window and lean events, each its own (docs/wire.md, "Windows"); one from before neither.
        use crate::harness::registry::View;
        assert_eq!(Takes::of(Some("z=gzip&window=1&lean=1")).view, View { window: true, lean: true });
        assert_eq!(Takes::of(Some("window=1")).view, View { window: true, lean: false });
        assert_eq!(Takes::of(Some("patch=state&window=2&leanest=1")).view, View::default());
    }

    /// W21-R15: a page from the conversation store is cut as the copy's are (whole turns, a page's
    /// bounds), and says there's more when it left some out or the store may have more.
    #[test]
    fn w21_r15_a_store_page_is_cut_as_the_copys_are() {
        let ev = |seq: u64, kind: &str| json!({"id": format!("e{seq}"), "seq": seq, "at": 0, "kind": kind, "text": "x"});
        // Thirty turns of three: a page holds twenty, from a prompt.
        let events: Vec<serde_json::Value> = (1..=90u64).map(|s| ev(s, if s % 3 == 1 { "user" } else { "assistant" })).collect();
        let p = store_page(events.clone(), 90, 1);
        assert_eq!(p.events.len(), 60);
        assert_eq!(p.window, crate::wire::ui::EventsWindow { before: Some(31), more: true });
        // All of what was read fits, fewer than asked for: nothing more.
        let few = store_page(events.get(..9).unwrap_or_default().to_vec(), 9, 1);
        assert_eq!((few.events.len(), few.window), (9, crate::wire::ui::EventsWindow { before: Some(1), more: false }));
        // As many as asked for: there may be more, unless it reached where the timeline begins.
        let full: Vec<serde_json::Value> = (11..11 + crate::harness::window::ROWS as u64).map(|s| ev(s, "assistant")).collect();
        assert!(store_page(full.clone(), crate::harness::window::ROWS, 1).window.more);
        assert!(!store_page(full, crate::harness::window::ROWS, 11).window.more);
        assert_eq!(store_page(Vec::new(), 0, 1).window, crate::wire::ui::EventsWindow::default());
    }

    #[test]
    fn a_packed_snapshot_unpacks_to_its_frames_then_synced() -> anyhow::Result<()> {
        use crate::harness::core::Update;
        use crate::harness::model::{Body, Event, State};
        use std::io::Read as _;
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = crate::harness::registry::Location::Machine("box".into());
            let info = crate::harness::session::Info { id: "s".into(), kind: "claude".into(), ..crate::harness::session::Info::default() };
            hub.harness.ingest(&hub, &there, "s", Update::Open { info: Box::new(info), since: None }).await;
            hub.harness.ingest(&hub, &there, "s", Update::State { state: Box::new(State::default()) }).await;
            let long = format!("{}\nsecond line", "word ".repeat(4000));
            let said = |n: u64, text: &str| Event { id: format!("e{n}"), seq: n, at: 1, parent: None, by: None, body: Body::Assistant { text: text.into(), streaming: false } };
            hub.harness.ingest(&hub, &there, "s", Update::Events { events: vec![said(1, &long), said(2, "short")] }).await;
            let att = hub.harness.attach("s").ok_or_else(|| anyhow::anyhow!("no session"))?;
            let frames = att.frames();
            let packed = att.snapshot.packed(&synced())?;
            // A long conversation packs small.
            assert!(packed.len() * 10 < long.len(), "{} bytes packed from {}", packed.len(), long.len());
            let mut text = String::new();
            flate2::read::GzDecoder::new(packed.as_slice()).read_to_string(&mut text)?;
            let lines: Vec<&str> = text.split('\n').collect();
            let mut want: Vec<String> = frames.clone();
            want.push(synced());
            assert_eq!(lines, want);
            assert!(frames.iter().any(|f| f.contains("second line")), "{frames:?}");
            Ok(())
        })
    }

    /// Goal 10, plan 01a1a87b2dae step 3: a long session's packed snapshot, put together from its
    /// chunks' pieces packed once (the sealed ones), its newest deflated as it's taken and what's between
    /// them stored, unpacks with flate2 to exactly what the whole gzip of its frames and `synced` held
    /// before; and taken again, it takes the pieces as they were packed and unpacks the same.
    #[test]
    fn g10_s3_a_snapshot_put_together_from_packed_chunks_unpacks_as_the_whole_gzip_did() -> anyhow::Result<()> {
        use crate::harness::core::Update;
        use crate::harness::model::{Body, Event, State};
        use std::io::{Read as _, Write as _};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = crate::harness::registry::Location::Machine("box".into());
            let info = crate::harness::session::Info { id: "s".into(), kind: "claude".into(), ..crate::harness::session::Info::default() };
            hub.harness.ingest(&hub, &there, "s", Update::Open { info: Box::new(info), since: None }).await;
            hub.harness.ingest(&hub, &there, "s", Update::State { state: Box::new(State::default()) }).await;
            let said = |n: u64| Event { id: format!("e{n}"), seq: n, at: 1, parent: None, by: None, body: Body::Assistant { text: format!("{n}: {}", "the retry loop reads its deadline ".repeat(usize::try_from(n % 7 + 1).unwrap_or(1))), streaming: false } };
            // Past two chunks, a third begun.
            hub.harness.ingest(&hub, &there, "s", Update::Events { events: (1..=150).map(said).collect() }).await;
            let unpack = |b: &[u8]| -> anyhow::Result<String> {
                let mut text = String::new();
                flate2::read::GzDecoder::new(b).read_to_string(&mut text)?;
                Ok(text)
            };
            let att = hub.harness.attach("s").ok_or_else(|| anyhow::anyhow!("no session"))?;
            let frames = att.frames();
            assert!(frames.iter().filter(|f| f.starts_with(r#"{"t":"events""#)).count() >= 3, "a frame a chunk: {}", frames.len());
            let mut whole = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            whole.write_all(format!("{}\n{}", frames.join("\n"), synced()).as_bytes())?;
            let was = unpack(&whole.finish()?)?;
            let packed = att.snapshot.packed(&synced())?;
            assert_eq!(unpack(&packed)?, was, "what the whole gzip held");
            let again = hub.harness.attach("s").ok_or_else(|| anyhow::anyhow!("no session"))?.snapshot.packed(&synced())?;
            assert_eq!(unpack(&again)?, was, "taken again, the same");
            assert!(packed.len() * 5 < was.len(), "it packs: {} of {}", packed.len(), was.len());
            Ok(())
        })
    }

    /// Goal 10, plan 01a1a87b2dae step 2: the reads of agent sessions the lab makes come from the
    /// copies' views as their owners encoded them, and say what they said when each read encoded
    /// its answer: the list and one session as `srv::json` wrote their infos (a state's change
    /// encoded again), and a session's newest events as the conversation store answers them; a copy
    /// that lacks some of those asked for leaves them to the store.
    #[test]
    fn g10_s2_session_reads_come_from_the_views_as_they_were_written() -> anyhow::Result<()> {
        use crate::harness::core::Update;
        use crate::harness::model::{Body, Event, State};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = crate::harness::registry::Location::Machine("box".into());
            for (id, at) in [("b", 2), ("a", 1)] {
                let info = crate::harness::session::Info { id: id.into(), kind: "claude".into(), created_at: at, ..crate::harness::session::Info::default() };
                hub.harness.ingest(&hub, &there, id, Update::Open { info: Box::new(info), since: None }).await;
            }
            let said = |n: u64| Event { id: format!("e{n}"), seq: n, at: 1, parent: None, by: None, body: Body::Assistant { text: format!("{n}: the retry loop reads its \"deadline\""), streaming: false } };
            hub.harness.ingest(&hub, &there, "a", Update::Events { events: (1..=120).map(said).collect() }).await;
            hub.harness.ingest(&hub, &there, "a", Update::State { state: Box::new(State::default()) }).await;
            // The second's copy holds its timeline from its fifth event on.
            hub.harness.ingest(&hub, &there, "b", Update::Events { events: (5..=20).map(said).collect() }).await;
            assert_eq!(hub.harness.list_json().as_ref(), serde_json::to_vec(&hub.harness.list())?.as_slice(), "the list, oldest first");
            let a = hub.harness.list().into_iter().find(|i| i.id == "a").ok_or_else(|| anyhow::anyhow!("no session a"))?;
            assert!(a.last_activity > 0, "the state changed its info");
            assert_eq!(hub.harness.info_json("a").as_deref(), Some(serde_json::to_vec(&a)?.as_slice()), "one, as it is now");
            assert!(hub.harness.info_json("nobody").is_none());
            let store = hub.harness.convo.clone().ok_or_else(|| anyhow::anyhow!("no conversation store"))?;
            crate::rt::blocking({
                let store = store.clone();
                move || store.sync()
            })
            .await??;
            // As `conversation_events` answers from the store.
            let stored = |id: &str, before: Option<u64>, limit: usize| -> anyhow::Result<Vec<u8>> {
                let events: Vec<serde_json::Value> = store.events(id, before, limit)?.iter().filter_map(|r| serde_json::from_str(&r.body).ok()).collect();
                Ok(serde_json::to_vec(&json!({ "events": events }))?)
            };
            for (before, limit) in [(None, 50), (Some(100), 50), (None, 1000), (Some(30), 10), (Some(1), 5)] {
                let copy = hub.harness.events_json("a", before, limit).ok_or_else(|| anyhow::anyhow!("the copy didn't say {before:?}, {limit}"))?;
                assert_eq!(String::from_utf8_lossy(&copy), String::from_utf8_lossy(&stored("a", before, limit)?), "{before:?}, {limit}");
            }
            assert_eq!(hub.harness.events_json("b", None, 10).as_deref(), Some(stored("b", None, 10)?.as_slice()), "ten of the sixteen it holds");
            assert!(hub.harness.events_json("b", None, 50).is_none(), "fewer than asked, its first four elsewhere: the store's to say");
            assert!(hub.harness.events_json("nobody", None, 50).is_none());
            Ok(())
        })
    }

    /// A response's body, whole.
    async fn body_of(r: Response) -> anyhow::Result<Vec<u8>> {
        use http_body_util::BodyExt as _;
        Ok(r.into_body().collect().await?.to_bytes().to_vec())
    }

    /// Goal 10, plan 01a1a87b2dae step 2, traced: once the reads kept in memory are made, the reads the
    /// lab makes of agents' pages (the sessions, one session, its newest events, its plans, its goals
    /// and an agent's with none) never ask the store, each answering the bytes the store's path makes;
    /// a write to its goals sends the next read to the store, which says what it holds, and the kept
    /// read is made again.
    #[test]
    fn g10_s2_the_labs_reads_never_ask_the_store() -> anyhow::Result<()> {
        use crate::harness::core::Update;
        use crate::harness::model::{Body, Event, State};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = crate::harness::registry::Location::Machine("box".into());
            let info = crate::harness::session::Info { id: "a".into(), kind: "claude".into(), ..crate::harness::session::Info::default() };
            hub.harness.ingest(&hub, &there, "a", Update::Open { info: Box::new(info), since: None }).await;
            let said = |n: u64| Event { id: format!("e{n}"), seq: n, at: 1, parent: None, by: None, body: Body::Assistant { text: format!("{n}: the deadline"), streaming: false } };
            hub.harness.ingest(&hub, &there, "a", Update::Events { events: (1..=60).map(said).collect() }).await;
            hub.harness.ingest(&hub, &there, "a", Update::State { state: Box::new(State::default()) }).await;
            let source = crate::plans::Source { session: Some("a".into()), ..crate::plans::Source::default() };
            crate::plans::create(&hub, crate::plans::New { body: "1. Measure the reads\n2. Prove it".into(), tags: vec!["Lab".into()], source: Some(source), ..crate::plans::New::default() }, "admin").await?;
            crate::plans::create(&hub, crate::plans::New { body: "Another's".into(), ..crate::plans::New::default() }, "admin").await?;
            crate::goals::add(&hub, "a", vec!["reads from memory".into()], false).await?;
            let q = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect::<HashMap<String, String>>();
            let api = |e: ApiError| anyhow::anyhow!("{} {}", e.0, e.1);
            // Made (the reads meanwhile are the store's), and then, while nothing writes, the reads ask
            // it nothing. A write of the hub's own between the two makes them stale: tried again.
            let deadline = Instant::now() + Duration::from_secs(30);
            let fresh = || hub.harness.views.plans.fresh(&hub.store, crate::plans::listed).is_some() && hub.harness.views.goals.fresh(&hub.store, crate::goals::stacks).is_some();
            let queries = [q(&[("session", "a")]), q(&[]), q(&[("brief", "1")]), q(&[("q", "MEASURE")]), q(&[("tag", "Lab")]), q(&[("q", "  ")]), q(&[("session", "nobody")])];
            let (list, one, events, plans, goals, none) = loop {
                while !fresh() {
                    anyhow::ensure!(Instant::now() < deadline, "the reads kept in memory weren't made");
                    crate::rt::sleep(Duration::from_millis(5)).await;
                }
                let asked = hub.store.asked();
                // What every read of an agent's page passes first (`authorized`): whether its caller sees it.
                crate::harness::lineage::may_see(&hub, &crate::authz::Caller::owner(), "a").await.map_err(|(c, m)| anyhow::anyhow!("{c} {m}"))?;
                let list = body_of(harness_sessions(&hub, &crate::authz::Caller::owner()).await).await?;
                let one = hub.harness.info_json("a").ok_or_else(|| anyhow::anyhow!("no session a"))?;
                let events = body_of(conversation_events(&hub, "a", &q(&[("limit", "50")])).await.map_err(api)?).await?;
                let mut plans = Vec::new();
                for query in &queries {
                    plans.push(body_of(plans_list(&hub, query).await.map_err(api)?).await?);
                }
                let goals = body_of(harness_goals(&hub, "a").await.map_err(api)?).await?;
                let none = body_of(harness_goals(&hub, "nobody").await.map_err(api)?).await?;
                if hub.store.asked() == asked {
                    break (list, one, events, plans, goals, none);
                }
                anyhow::ensure!(Instant::now() < deadline, "the reads asked the store every time");
            };
            assert_eq!(list, serde_json::to_vec(&hub.harness.list())?);
            let a = hub.harness.list().into_iter().find(|i| i.id == "a").ok_or_else(|| anyhow::anyhow!("no session a in the list"))?;
            assert_eq!(one.as_ref(), serde_json::to_vec(&a)?.as_slice());
            assert!(events.starts_with(br#"{"events":[{"#), "{}", String::from_utf8_lossy(&events));
            for (query, got) in queries.iter().zip(&plans) {
                let p = crate::plans::list(&hub, query.get("q").cloned(), query.get("tag").cloned()).await?;
                let p = crate::plans::of_source(p, query.get("session").map(String::as_str).filter(|s| !s.is_empty()), false);
                let want = if query.contains_key("brief") { serde_json::to_vec(&p.iter().map(crate::plans::brief).collect::<Vec<_>>())? } else { serde_json::to_vec(&p)? };
                assert_eq!(String::from_utf8_lossy(got), String::from_utf8_lossy(&want), "{query:?}");
            }
            assert_eq!(goals, serde_json::to_vec(&crate::goals::get(&hub, "a").await?)?);
            assert_eq!(none, serde_json::to_vec(&crate::goals::get(&hub, "nobody").await?)?);
            // A goal added: the next read asks the store, which has it; then the stacks are made again.
            crate::goals::add(&hub, "a", vec!["and then prove it".into()], false).await?;
            let asked = hub.store.asked();
            let goals = body_of(harness_goals(&hub, "a").await.map_err(api)?).await?;
            assert!(hub.store.asked() > asked, "stale: the store answered");
            assert_eq!(goals, serde_json::to_vec(&crate::goals::get(&hub, "a").await?)?);
            assert!(String::from_utf8_lossy(&goals).contains("and then prove it"));
            while hub.harness.views.goals.fresh(&hub.store, crate::goals::stacks).is_none() {
                anyhow::ensure!(Instant::now() < deadline, "the goal stacks weren't made again");
                crate::rt::sleep(Duration::from_millis(5)).await;
            }
            assert_eq!(body_of(harness_goals(&hub, "a").await.map_err(api)?).await?, goals, "made again, as the store has it");
            Ok(())
        })
    }

    /// Goal 10, plan 01a1a87b2dae step 3: a packed snapshot goes in frames of at most
    /// [`SNAPSHOT_FRAME`] bytes (the first binary, the rest continuations, the last final), which a
    /// client joins into the one message they are, whatever its length (goal 11, plan 01a1a87b2e56 step 5:
    /// a frame what its socket writes at a time, a piece's several).
    #[test]
    fn g10_s3_a_packed_snapshot_goes_in_pieces_a_client_joins() -> anyhow::Result<()> {
        use std::io::Cursor;
        use tungstenite::protocol::{Role, WebSocket};
        for len in [0, 1, SNAPSHOT_FRAME - 1, SNAPSHOT_FRAME, SNAPSHOT_FRAME + 1, SNAPSHOT_PIECE, SNAPSHOT_PIECE + 1, 3 * SNAPSHOT_PIECE + 17] {
            let bytes: Vec<u8> = (0..len).map(|i| u8::try_from(i % 251).unwrap_or(0)).collect();
            let sent: Vec<ws::Message> = pieces(bytes.clone()).collect();
            assert_eq!(sent.len(), len.div_ceil(SNAPSHOT_FRAME).max(1), "{len} bytes' frames");
            assert!(sent.iter().all(|m| matches!(m, ws::Message::Frame(f) if f.payload().len() <= SNAPSHOT_FRAME)), "{len} bytes: a frame past {SNAPSHOT_FRAME}");
            let mut server = WebSocket::from_raw_socket(Cursor::new(Vec::new()), Role::Server, None);
            for m in sent {
                server.send(m)?;
            }
            let wire = server.get_ref().get_ref().clone();
            let mut client = WebSocket::from_raw_socket(Cursor::new(wire), Role::Client, None);
            match client.read()? {
                ws::Message::Binary(got) => assert!(got.as_ref() == bytes.as_slice(), "{len} bytes, joined: {} came", got.len()),
                other => anyhow::bail!("{len} bytes came as {other:?}"),
            }
        }
        Ok(())
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: a message packed as it goes out, sent as its pieces come (each in
    /// frames of at most [`SNAPSHOT_PIECE`]: the first piece's first binary, every other a continuation, the
    /// last piece's last final), is the one message a client joins, whatever the pieces' sizes.
    #[test]
    fn g11_s5_a_snapshots_pieces_as_they_come_make_one_message() -> anyhow::Result<()> {
        use std::io::Cursor;
        use tungstenite::protocol::{Role, WebSocket};
        for sizes in [vec![10], vec![SNAPSHOT_PIECE + 5, 7], vec![3, SNAPSHOT_PIECE, 2 * SNAPSHOT_PIECE + 1, 0, 9]] {
            let parts: Vec<Vec<u8>> = sizes.iter().enumerate().map(|(k, n)| (0..*n).map(|i| u8::try_from((i + k) % 251).unwrap_or(0)).collect()).collect();
            let mut server = WebSocket::from_raw_socket(Cursor::new(Vec::new()), Role::Server, None);
            let last = parts.len().saturating_sub(1);
            for (k, piece) in parts.iter().enumerate() {
                for frame in pieces_of(piece.clone(), k == 0, k == last) {
                    server.send(frame)?;
                }
            }
            let wire = server.get_ref().get_ref().clone();
            let mut client = WebSocket::from_raw_socket(Cursor::new(wire), Role::Client, None);
            let whole = parts.concat();
            match client.read()? {
                ws::Message::Binary(got) => anyhow::ensure!(got.as_ref() == whole.as_slice(), "pieces of {sizes:?}: {} bytes came of {}", got.len(), whole.len()),
                other => anyhow::bail!("pieces of {sizes:?} came as {other:?}"),
            }
        }
        Ok(())
    }

    /// A GET of `path` as the lab's readers send it (crates/kiln-lab's `reader`): the owner's, by the
    /// admin token, taking nothing packed.
    fn lab_get(path: &str) -> anyhow::Result<Request> {
        Ok(http::Request::builder().method(Method::GET).uri(path).header(header::AUTHORIZATION, "Bearer test-admin-token").header(header::ACCEPT_ENCODING, "identity").body(srv::Incoming::empty())?)
    }

    /// Goal 10, plan 01a1a87b2dae step 2, traced: every read the lab's realistic profile makes
    /// (crates/kiln-lab's `reader`: the sessions, one session, its newest events, the runners, who's
    /// asking, the overview, the runs, an agent's plans, its goals and an unknown agent's), sent to the
    /// API as a request, asks no store and encodes no JSON while it's answered once what's kept is made
    /// (`crate::reads::trace`), each saying what the store's path says. A runner's live record changed,
    /// the runners' list is made again off the request path and the read after it encodes nothing; a
    /// run added sends the next read of the runs to the store, which has it, and the list is kept again.
    #[test]
    fn g10_s2_every_read_the_lab_makes_asks_no_store_and_encodes_nothing() -> anyhow::Result<()> {
        use crate::harness::core::Update;
        use crate::harness::model::{Body, Event, State};
        use crate::http::server::Handler as _;
        use crate::reads::trace::{Counts, traced};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            // An agent at work, with a plan and a goal.
            let there = crate::harness::registry::Location::Machine("box".into());
            let info = crate::harness::session::Info { id: "a".into(), kind: "claude".into(), ..crate::harness::session::Info::default() };
            hub.harness.ingest(&hub, &there, "a", Update::Open { info: Box::new(info), since: None }).await;
            let said = |n: u64| Event { id: format!("e{n}"), seq: n, at: 1, parent: None, by: None, body: Body::Assistant { text: format!("{n}: the deadline"), streaming: false } };
            hub.harness.ingest(&hub, &there, "a", Update::Events { events: (1..=60).map(said).collect() }).await;
            hub.harness.ingest(&hub, &there, "a", Update::State { state: Box::new(State::default()) }).await;
            let source = crate::plans::Source { session: Some("a".into()), ..crate::plans::Source::default() };
            crate::plans::create(&hub, crate::plans::New { body: "1. Measure the reads\n2. Prove it".into(), source: Some(source), ..crate::plans::New::default() }, "admin").await?;
            crate::goals::add(&hub, "a", vec!["reads from memory".into()], false).await?;
            // A runner (its row, its live record) and a repository's run.
            let row = crate::store::AgentRow { name: "box".into(), labels: vec![], slots: 1, hostname: "box".into(), os: "linux".into(), arch: "x86_64".into(), version: String::new(), ephemeral: false, connected: true, last_seen: 0, enrolled_at: 0 };
            hub.store.upsert_agent(row).await?;
            hub.put_agent(crate::hub::tests::live_runner("box", "linux", "x86_64"));
            let repo = Repo { id: "r1".into(), name: "acme/app".into(), clone_url: "https://example/app".into(), branches: vec![], poll_seconds: 60, enabled: true, created_at: 0, last_polled_at: None, last_error: None, mode: "upstream".into(), default_branch: None, mirror: None };
            let repo = hub.store.upsert_repo(repo).await?;
            let run = |id: &str, at: i64| crate::store::Run {
                id: id.into(),
                repo_id: repo.id.clone(),
                repo_name: repo.name.clone(),
                number: 0,
                pipeline: "ci".into(),
                event: "push".into(),
                branch: "main".into(),
                sha: "abc".into(),
                message: String::new(),
                author: String::new(),
                status: crate::store::Status::Succeeded,
                created_at: at,
                started_at: None,
                finished_at: None,
                error: None,
                concurrency_group: None,
            };
            hub.store.create_run(run("run1", 1), Vec::new()).await?;
            let api = Arc::new(Api { hub: Arc::clone(&hub) });
            let read = |path: &'static str| {
                let api = Arc::clone(&api);
                async move {
                    let (r, took) = traced(api.handle(lab_get(path)?)).await;
                    anyhow::Ok((r.status(), body_of(r).await?, took))
                }
            };
            let paths = [
                "/api/harness/sessions",
                "/api/harness/sessions/a",
                "/api/harness/sessions/a/events?limit=50",
                "/api/agents",
                "/api/me",
                "/api/overview",
                "/api/runs",
                "/api/plans?session=a",
                "/api/harness/sessions/a/goals",
                "/api/harness/sessions/nobody/goals",
            ];
            // Made (the reads meanwhile are the store's, or make what's kept ahead), and then each read
            // asks nothing of a store and encodes nothing. A change of the hub's own between makes some
            // stale again: they're all read again.
            let deadline = Instant::now() + Duration::from_secs(30);
            let answers = loop {
                let (mut answers, mut busy) = (HashMap::new(), Vec::new());
                for path in paths {
                    let (status, body, took) = read(path).await?;
                    anyhow::ensure!(status == StatusCode::OK, "{path}: {status} {}", String::from_utf8_lossy(&body));
                    if took != Counts::default() {
                        busy.push((path, took));
                    }
                    answers.insert(path, body);
                }
                if busy.is_empty() {
                    break answers;
                }
                anyhow::ensure!(Instant::now() < deadline, "reads that still asked a store or encoded: {busy:?}");
                crate::rt::sleep(Duration::from_millis(5)).await;
            };
            let got = |path: &str| answers.get(path).cloned().unwrap_or_default();
            let owner = crate::authz::Caller::owner();
            // Each says what the store's path says.
            assert_eq!(got("/api/harness/sessions"), serde_json::to_vec(&hub.harness.list())?);
            let a = hub.harness.list().into_iter().find(|i| i.id == "a").ok_or_else(|| anyhow::anyhow!("no session a"))?;
            assert_eq!(got("/api/harness/sessions/a"), serde_json::to_vec(&a)?);
            let events: serde_json::Value = serde_json::from_slice(&got("/api/harness/sessions/a/events?limit=50"))?;
            let seqs: Vec<u64> = events.get("events").and_then(serde_json::Value::as_array).into_iter().flatten().filter_map(|e| e.get("seq").and_then(serde_json::Value::as_u64)).collect();
            assert_eq!(seqs, (11..=60).collect::<Vec<u64>>(), "its newest fifty, oldest first");
            assert_eq!(got("/api/agents"), serde_json::to_vec(&crate::doctor::runners_for(&hub, &owner).await)?);
            assert_eq!(got("/api/me"), serde_json::to_vec(&me_json(&hub, Some(&owner), Vec::new()))?);
            // The uptime may have gone on a second between the two: compared without it.
            let without_uptime = |mut v: serde_json::Value| {
                if let Some(system) = v.get_mut("system").and_then(serde_json::Value::as_object_mut) {
                    system.remove("uptime_s");
                }
                v
            };
            let stored = overview_json(&hub, &hub.store.stats(None).await?, &hub.agent_list().await, &system_now(&hub), queue_now());
            assert_eq!(without_uptime(serde_json::from_slice(&got("/api/overview"))?), without_uptime(stored));
            assert_eq!(got("/api/runs"), serde_json::to_vec(&crate::reads::runs(&hub.store, &HashMap::new(), None).await?)?);
            let plans = crate::plans::of_source(crate::plans::list(&hub, None, None).await?, Some("a"), false);
            assert_eq!(got("/api/plans?session=a"), serde_json::to_vec(&plans)?);
            assert_eq!(got("/api/harness/sessions/a/goals"), serde_json::to_vec(&crate::goals::get(&hub, "a").await?)?);
            assert_eq!(got("/api/harness/sessions/nobody/goals"), serde_json::to_vec(&crate::goals::get(&hub, "nobody").await?)?);
            // A runner's live record changed: its maker makes the list again before any read, and the
            // read after that encodes nothing and says the change. (A write of the hub's own that sends
            // that read to the store: the record changed again.)
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut busy = 0;
            let agents = loop {
                busy += 1;
                hub.change_agent("box", |a| a.busy = busy);
                while !agents_key(&hub).is_some_and(|k| hub.harness.views.agents.holds(&k)) {
                    anyhow::ensure!(Instant::now() < deadline, "the runners' list wasn't made again");
                    crate::rt::sleep(Duration::from_millis(5)).await;
                }
                let (_, agents, took) = read("/api/agents").await?;
                if took == Counts::default() {
                    break agents;
                }
                anyhow::ensure!(Instant::now() < deadline, "the reads of the runners' list made ahead still asked or encoded: {took:?}");
            };
            assert_eq!(agents, serde_json::to_vec(&crate::doctor::runners_for(&hub, &owner).await)?);
            assert!(String::from_utf8_lossy(&agents).contains(&format!(r#""busy":{busy}"#)), "{}", String::from_utf8_lossy(&agents));
            // A run added: the next read of the runs asks the store, which has it; then it's kept again.
            hub.store.create_run(run("run2", 2), Vec::new()).await?;
            let (_, runs, took) = read("/api/runs").await?;
            assert!(took.asked > 0, "stale: the store answered");
            assert!(String::from_utf8_lossy(&runs).contains("run2"), "{}", String::from_utf8_lossy(&runs));
            loop {
                if hub.harness.views.runs.fresh(&hub.store, crate::reads::runs_listed).is_some() {
                    let (_, again, took) = read("/api/runs").await?;
                    if took == Counts::default() {
                        assert_eq!(again, runs, "kept again, as the store has it");
                        break;
                    }
                }
                anyhow::ensure!(Instant::now() < deadline, "the runs' list wasn't kept again");
                crate::rt::sleep(Duration::from_millis(5)).await;
            }
            Ok(())
        })
    }

    /// `fut`'s output, and the segments its polls inflated on their own thread (`crate::harness::packed::INFLATED`):
    /// what a read inflated on its shard.
    async fn inflated_in<F: Future>(fut: F) -> (F::Output, u64) {
        let mut fut = std::pin::pin!(fut);
        let mut inflated = 0u64;
        let out = std::future::poll_fn(|cx| {
            let before = crate::harness::packed::INFLATED.with(std::cell::Cell::get);
            let polled = fut.as_mut().poll(cx);
            inflated = inflated.wrapping_add(crate::harness::packed::INFLATED.with(std::cell::Cell::get).wrapping_sub(before));
            polled
        })
        .await;
        (out, inflated)
    }

    /// Goal 10, plan 01a1a87b2dae step 5: a read of a session's newest events whose compact chunk's texts no read
    /// kept yet inflates nothing on its shard: they're inflated on the blocking pool and kept, and the read after
    /// takes them as they are; both answer what the copy's page says, byte for byte.
    #[test]
    fn g10_s5_an_events_read_inflates_nothing_on_its_shard() -> anyhow::Result<()> {
        use crate::harness::copies::Unread;
        use crate::harness::core::Update;
        use crate::harness::model::{Body, Event};
        use crate::http::server::Handler as _;
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = crate::harness::registry::Location::Machine("box".into());
            let info = crate::harness::session::Info { id: "a".into(), kind: "claude".into(), ..crate::harness::session::Info::default() };
            hub.harness.ingest(&hub, &there, "a", Update::Open { info: Box::new(info), since: None }).await;
            // Two chunks' events and nine: the two sealed are packed off the owner's shard and kept compact, and the
            // newest fifty reach into the second. A chunk's events, from the bound of what a copy keeps packed (two
            // chunks' at a packed event's bytes).
            let chunk = crate::harness::copies::PAGE_BYTES / (2 * crate::harness::copies::PACKED_EVENT_BYTES);
            let n = u64::try_from(2 * chunk + 9)?;
            let said = |q: u64| Event { id: format!("e{q}"), seq: q, at: 1, parent: None, by: None, body: Body::Assistant { text: format!("{q}: the deadline"), streaming: false } };
            hub.harness.ingest(&hub, &there, "a", Update::Events { events: (1..=n).map(said).collect() }).await;
            // Till its chunks are compact, the page is read from the events themselves.
            let deadline = Instant::now() + Duration::from_secs(30);
            while hub.harness.events_page_kept("a", None, 50) != Err(Unread::Unkept) {
                anyhow::ensure!(Instant::now() < deadline, "the copy's sealed chunks weren't kept compact");
                crate::rt::sleep(Duration::from_millis(5)).await;
            }
            let api = Arc::new(Api { hub: Arc::clone(&hub) });
            let read = || {
                let api = Arc::clone(&api);
                async move {
                    let (r, inflated) = inflated_in(api.handle(lab_get("/api/harness/sessions/a/events?limit=50")?)).await;
                    anyhow::Ok((r.status(), body_of(r).await?, inflated))
                }
            };
            let (status, first, inflated) = read().await?;
            anyhow::ensure!(status == StatusCode::OK && inflated == 0, "the first read: {status}, {inflated} inflated on its shard");
            anyhow::ensure!(hub.harness.events_page_kept("a", None, 50).is_ok(), "its texts kept for the reads after it");
            let (status, again, inflated) = read().await?;
            anyhow::ensure!(status == StatusCode::OK && inflated == 0 && again == first, "the read after: {status}, {inflated} inflated");
            let copy = hub.harness.events_json("a", None, 50).ok_or_else(|| anyhow::anyhow!("the copy's page"))?;
            anyhow::ensure!(first == copy.as_ref(), "the copy's page, byte for byte");
            let events: serde_json::Value = serde_json::from_slice(&first)?;
            let seqs: Vec<u64> = events.get("events").and_then(serde_json::Value::as_array).into_iter().flatten().filter_map(|e| e.get("seq").and_then(serde_json::Value::as_u64)).collect();
            assert_eq!(seqs, (n - 49..=n).collect::<Vec<u64>>(), "its newest fifty, oldest first");
            Ok(())
        })
    }

    /// Goal 10, plan 01a1a87b2dae step 5: the conversation store's page of a session's events, now written out on the
    /// blocking pool (`stored_events`), is the bytes the request's shard wrote: each row's body read as JSON, one that
    /// isn't left out, as `srv::json` writes an answer.
    #[test]
    fn g10_s5_the_stores_events_page_is_written_as_it_was() -> anyhow::Result<()> {
        let row = |seq: u64, body: &str| crate::convo::rows::EventRow {
            session: "a".into(),
            id: format!("e{seq}"),
            seq,
            at: 1,
            parent: None,
            by: None,
            kind: "assistant".into(),
            level: None,
            tool: None,
            status: None,
            text: String::new(),
            body: body.into(),
        };
        let rows = vec![
            row(1, r#"{"id":"e1","seq":1,"kind":"assistant","text":"the retry loop é \"reads\""}"#),
            row(2, "not json"),
            row(3, r#"{ "seq": 3, "id": "e3", "kind": "tool", "input": {"b": 2, "a": [1, 2.50, null]} }"#),
        ];
        let made = stored_events(&rows)?;
        let events: Vec<serde_json::Value> = rows.iter().filter_map(|r| serde_json::from_str(&r.body).ok()).collect();
        let was = crate::rt::block_on(async move { body_of(srv::json(StatusCode::OK, &json!({ "events": events }))).await })?;
        assert_eq!(made.as_ref(), was.as_slice());
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&made)?.get("events").and_then(serde_json::Value::as_array).map(Vec::len), Some(2), "the row that isn't JSON left out");
        Ok(())
    }

    /// Goal 10, plan 01a1a87b2dae step 5: the overview counts its runners from their rows and live records as they're
    /// kept, none copied, as their list counts them (a row with a live record, connected or not; a row without one; a
    /// live record without a row), and says the bytes it said when it made their list to count them.
    #[test]
    fn g10_s5_the_overview_counts_its_runners_as_their_list_does() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let row = |name: &str, slots: u32| crate::store::AgentRow { name: name.into(), labels: vec![], slots, hostname: name.into(), os: "linux".into(), arch: "x86_64".into(), version: String::new(), ephemeral: false, connected: false, last_seen: 0, enrolled_at: 0 };
            for (name, slots) in [("box", 4), ("attic", 3), ("shed", 5)] {
                hub.store.upsert_agent(row(name, slots)).await?;
            }
            let live = |name: &str, connected: bool, slots: u32, busy: u32| crate::hub::AgentLive { connected, slots, busy, ..crate::hub::tests::live_runner(name, "linux", "x86_64") };
            // box: live and connected; attic: no live record; shed: live, not connected; ghost: live with no row.
            hub.put_agent(live("box", true, 4, 3));
            hub.put_agent(live("shed", false, 5, 2));
            hub.put_agent(live("ghost", true, 2, 1));
            let deadline = Instant::now() + Duration::from_secs(30);
            let key = loop {
                if let Some(key) = overview_key(&hub) {
                    break key;
                }
                anyhow::ensure!(Instant::now() < deadline, "the overview's figures and rows weren't kept");
                crate::rt::sleep(Duration::from_millis(5)).await;
            };
            let rows = &key.rows.1;
            let list = hub.agent_list_of(rows.rows.clone(), &rows.labels, &rows.machines, &key.live);
            let counted = RunnerCounts::of_rows(&rows.rows, &key.live);
            assert_eq!(counted, RunnerCounts::of_list(&list), "as the list counts them");
            assert_eq!(counted, RunnerCounts { connected: 2, total: 4, slots: 6, busy: 4 });
            let made = overview_made(&hub, &key)?;
            // As it was written before (`overview_json` of the runners' list).
            let (stats, system, (queued, running)) = (&key.stats.1.json, &key.system, key.queue);
            let was = crate::reads::encode(&json!({
                "stats": stats,
                "queue": { "queued": queued, "running": running },
                "agents": {
                    "connected": list.iter().filter(|a| a.connected).count(),
                    "total": list.len(),
                    "slots": list.iter().filter(|a| a.connected).map(|a| a.slots).sum::<u32>(),
                    "busy": list.iter().filter(|a| a.connected).map(|a| a.busy).sum::<u32>(),
                },
                "system": system_of(&hub, system),
                "sample": system.sample,
            }))?;
            assert_eq!(made, was, "the bytes it said when it counted the list");
            Ok(())
        })
    }

    /// A WebSocket client of the API at `port` on `path`, as the owner; with `rcvbuf`, its socket holds
    /// at most about that many bytes it hasn't read.
    fn ws_client(port: u16, path: &str, rcvbuf: Option<usize>) -> anyhow::Result<tungstenite::WebSocket<std::net::TcpStream>> {
        use tungstenite::client::IntoClientRequest as _;
        let sock = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None)?;
        if let Some(n) = rcvbuf {
            sock.set_recv_buffer_size(n)?;
        }
        sock.connect(&std::net::SocketAddr::from(([127, 0, 0, 1], port)).into())?;
        let mut req = format!("ws://127.0.0.1:{port}{path}").into_client_request()?;
        req.headers_mut().insert("authorization", "Bearer test-admin-token".parse()?);
        let (ws, _) = tungstenite::client::client(req, std::net::TcpStream::from(sock)).map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
        Ok(ws)
    }

    /// Goal 10, plan 01a1a87b2dae step 3, traced: a frame published while another viewer of its session
    /// attaches reaches the viewer already watching without waiting for that attach. The attaching one's
    /// socket takes its snapshot's first frame and nothing more, so its snapshot (megabytes) is still
    /// being written (`crate::reads::Views::attaching`) when the frame goes and when the other has it.
    /// The frame is one message on the session's feed, encoded once, and its bytes are what each viewer
    /// reads; the attaching one has it after its snapshot.
    #[test]
    fn g10_s3_a_frame_reaches_the_viewers_while_another_attaches() -> anyhow::Result<()> {
        use crate::harness::core::Update;
        use crate::harness::model::{Body, Event, State};
        use kiln_seams::Bus as _;
        use std::sync::atomic::Ordering;
        const MARK: &str = "g10-s3: published while another viewer attaches";
        // What a viewer's thread says: its snapshot came (false), or the frame (true, its text); or why it stopped.
        type Said = Result<(bool, String), String>;
        crate::rt::block_on(async {
            // kiln's API on a port of its own.
            let tcp = hyper_rt::tcp::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), 16).map_err(|e| anyhow::anyhow!("{e}"))?;
            let port = tcp.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?.port();
            let hub = crate::hub::tests::scratch_hub().await?;
            let api = Arc::new(Api { hub: Arc::clone(&hub) });
            crate::rt::spawn(async move {
                while let Ok(stream) = tcp.accept().await {
                    let (api, remote) = (Arc::clone(&api), stream.peer_addr().ok());
                    crate::rt::spawn(async move { crate::http::server::serve_accepted(stream, api, remote, None).await });
                }
            });
            // A long session: its snapshot is megabytes, far more than the sockets between hold.
            let there = crate::harness::registry::Location::Machine("box".into());
            let info = crate::harness::session::Info { id: "s".into(), kind: "claude".into(), ..crate::harness::session::Info::default() };
            hub.harness.ingest(&hub, &there, "s", Update::Open { info: Box::new(info), since: None }).await;
            hub.harness.ingest(&hub, &there, "s", Update::State { state: Box::new(State::default()) }).await;
            let said = |n: u64, text: String| Event { id: format!("e{n}"), seq: n, at: 1, parent: None, by: None, body: Body::Assistant { text, streaming: false } };
            let long: Vec<Event> = (1..=256).map(|n| said(n, format!("{n}: {}", "the retry loop reads its deadline ".repeat(960)))).collect();
            hub.harness.ingest(&hub, &there, "s", Update::Events { events: long }).await;
            let wait = Duration::from_secs(60);
            let next = |rx: async_channel::Receiver<Said>, what: &'static str| async move {
                match crate::rt::timeout(wait, rx.recv()).await {
                    Ok(Ok(Ok(said))) => Ok(said),
                    Ok(Ok(Err(why))) => Err(anyhow::anyhow!("{what}: {why}")),
                    Ok(Err(_)) => Err(anyhow::anyhow!("{what}: its viewer went")),
                    Err(_) => Err(anyhow::anyhow!("{what}: not within {wait:?}")),
                }
            };
            // Viewer A watches it, as the UI does (its snapshot packed).
            let (a_tx, a_rx) = async_channel::unbounded::<Said>();
            std::thread::spawn(move || {
                let watch = || -> anyhow::Result<String> {
                    let mut ws = ws_client(port, "/api/harness/sessions/s/ws?z=gzip&patch=state&patch=changes", None)?;
                    loop {
                        match ws.read()? {
                            tungstenite::Message::Binary(_) => {
                                let _ = a_tx.send_blocking(Ok((false, String::new())));
                            }
                            tungstenite::Message::Text(t) if t.as_str().contains(MARK) => return Ok(t.as_str().to_owned()),
                            _ => {}
                        }
                    }
                };
                let _ = a_tx.send_blocking(watch().map(|t| (true, t)).map_err(|e| format!("{e:#}")));
            });
            anyhow::ensure!(!next(a_rx.clone(), "A's snapshot").await?.0, "A's snapshot comes first");
            // Viewer B attaches: its socket takes its snapshot's first frame (its head) and nothing more
            // until it's told to read on.
            let (b_tx, b_rx) = async_channel::unbounded::<Said>();
            let (go, read_on) = async_channel::bounded::<()>(1);
            std::thread::spawn(move || {
                let attach = || -> anyhow::Result<String> {
                    let mut ws = ws_client(port, "/api/harness/sessions/s/ws", Some(4096))?;
                    let head = ws.read()?;
                    let _ = b_tx.send_blocking(Ok((false, head.to_text().unwrap_or_default().to_owned())));
                    let _ = read_on.recv_blocking();
                    loop {
                        if let tungstenite::Message::Text(t) = ws.read()?
                            && t.as_str().contains(MARK)
                        {
                            return Ok(t.as_str().to_owned());
                        }
                    }
                };
                let _ = b_tx.send_blocking(attach().map(|t| (true, t)).map_err(|e| format!("{e:#}")));
            });
            let (_, head) = next(b_rx.clone(), "B's snapshot's head").await?;
            anyhow::ensure!(!head.contains(MARK), "B's snapshot was taken before the frame");
            // B's snapshot is being written (A's is done), and stays so: B reads none of it.
            let writing = || hub.harness.views.attaching.load(Ordering::Acquire);
            let deadline = Instant::now() + wait;
            while writing() != 1 {
                anyhow::ensure!(Instant::now() < deadline, "{} snapshots being written, not B's alone", writing());
                crate::rt::sleep(Duration::from_millis(5)).await;
            }
            // The feed, as a viewer of its own reads it: where it ends now.
            let feed = hub.harness.attach("s").ok_or_else(|| anyhow::anyhow!("no session s"))?;
            let from = feed.at.ok_or_else(|| anyhow::anyhow!("the session's feed has no topic"))?;
            // The frame, published while B attaches: A has it, while B's snapshot is still being written.
            hub.harness.ingest(&hub, &there, "s", Update::Events { events: vec![said(257, MARK.to_owned())] }).await;
            let (live, a_has) = next(a_rx, "A's frame (it waited for B's attach)").await?;
            anyhow::ensure!(live, "A had the frame live");
            anyhow::ensure!(writing() == 1, "B's attach was under way as A had the frame: {} snapshots being written", writing());
            // Encoded once: one message on the feed carries it, and its bytes are what A read.
            let batch = hub.bus.read(&feed.topic, from, 64).await.map_err(|e| anyhow::anyhow!("{e}"))?;
            let carrying: Vec<String> = batch.messages.iter().filter_map(|m| crate::harness::live::decode_ref(&m.payload)).filter(|f| f.text.contains(MARK)).map(|f| f.text.to_owned()).collect();
            anyhow::ensure!(carrying == vec![a_has.clone()], "one message on the feed carries the frame, as A read it: {} do", carrying.len());
            // B reads on: its snapshot, then the frame, the same bytes.
            go.send(()).await?;
            let (live, b_has) = next(b_rx, "B's frame").await?;
            anyhow::ensure!(live && b_has == a_has, "B had the frame's bytes after its snapshot");
            while writing() != 0 {
                anyhow::ensure!(Instant::now() < deadline, "B's snapshot was never written whole");
                crate::rt::sleep(Duration::from_millis(5)).await;
            }
            drop(feed);
            Ok(())
        })
    }

    /// Goal 10, plan 01a1a87b2dae step 5: a session's viewer is served on its session's owner's shard, wherever its
    /// connection came in: its socket moves there with its upgrade, and its snapshot and live frames come over it; no
    /// viewer in this test process runs off its session's owner's shard. Here the session's owner is another shard than
    /// the one its connection was served on (where the runtime has another), so its socket moved.
    #[test]
    fn g10_s5_a_viewer_is_served_on_its_sessions_owner_shard() -> anyhow::Result<()> {
        use crate::harness::core::Update;
        use crate::harness::model::{Body, Event, State};
        use std::sync::atomic::Ordering;
        const MARK: &str = "g10-s5: a frame to a viewer on its session's owner's shard";
        crate::rt::block_on(async {
            // kiln's API on a port of its own, its connections served on this task's shard.
            let tcp = hyper_rt::tcp::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), 16).map_err(|e| anyhow::anyhow!("{e}"))?;
            let port = tcp.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?.port();
            let hub = crate::hub::tests::scratch_hub().await?;
            let api = Arc::new(Api { hub: Arc::clone(&hub) });
            let here = crate::rt::shard_index().ok_or_else(|| anyhow::anyhow!("not on a shard"))?;
            crate::rt::spawn(async move {
                while let Ok(stream) = tcp.accept().await {
                    let (api, remote) = (Arc::clone(&api), stream.peer_addr().ok());
                    crate::rt::spawn(async move { crate::http::server::serve_accepted(stream, api, remote, None).await });
                }
            });
            // A session another shard owns.
            let elsewhere = crate::rt::shard_count() > 1;
            let id = (0u64..1024).map(|n| format!("viewed-{n}")).find(|id| !elsewhere || hub.harness.owner_shard(id) != here).ok_or_else(|| anyhow::anyhow!("no session another shard owns"))?;
            anyhow::ensure!(!elsewhere || hub.harness.owner_shard(&id) != here, "{id} is owned where its viewer's connection is served");
            let there = crate::harness::registry::Location::Machine("box".into());
            let info = crate::harness::session::Info { id: id.clone(), kind: "claude".into(), ..crate::harness::session::Info::default() };
            hub.harness.ingest(&hub, &there, &id, Update::Open { info: Box::new(info), since: None }).await;
            hub.harness.ingest(&hub, &there, &id, Update::State { state: Box::new(State::default()) }).await;
            let said = |n: u64, text: &str| Event { id: format!("e{n}"), seq: n, at: 1, parent: None, by: None, body: Body::Assistant { text: text.to_owned(), streaming: false } };
            hub.harness.ingest(&hub, &there, &id, Update::Events { events: vec![said(1, "the retry loop reads its deadline")] }).await;
            let on0 = VIEWERS_ON_OWNER.get(1).map_or(0, |n| n.load(Ordering::Relaxed));
            // The viewer: its snapshot (its frames, then `synced`), then the live frame.
            let (tx, rx) = async_channel::unbounded::<Result<bool, String>>();
            let path = format!("/api/harness/sessions/{id}/ws");
            std::thread::spawn(move || {
                let watch = || -> anyhow::Result<()> {
                    let mut ws = ws_client(port, &path, None)?;
                    loop {
                        if let tungstenite::Message::Text(t) = ws.read()? {
                            if t.as_str() == synced() {
                                let _ = tx.send_blocking(Ok(false));
                            } else if t.as_str().contains(MARK) {
                                return Ok(());
                            }
                        }
                    }
                };
                let _ = tx.send_blocking(watch().map(|()| true).map_err(|e| format!("{e:#}")));
            });
            let wait = Duration::from_secs(60);
            let next = |what: &'static str| {
                let rx = rx.clone();
                async move {
                    match crate::rt::timeout(wait, rx.recv()).await {
                        Ok(Ok(Ok(live))) => Ok(live),
                        Ok(Ok(Err(why))) => Err(anyhow::anyhow!("{what}: {why}")),
                        Ok(Err(_)) => Err(anyhow::anyhow!("{what}: its viewer went")),
                        Err(_) => Err(anyhow::anyhow!("{what}: not within {wait:?}")),
                    }
                }
            };
            anyhow::ensure!(!next("its snapshot").await?, "its snapshot comes first");
            hub.harness.ingest(&hub, &there, &id, Update::Events { events: vec![said(2, MARK)] }).await;
            anyhow::ensure!(next("its live frame").await?, "the live frame");
            let (off, on) = (VIEWERS_ON_OWNER.first().map_or(0, |n| n.load(Ordering::Relaxed)), VIEWERS_ON_OWNER.get(1).map_or(0, |n| n.load(Ordering::Relaxed)));
            anyhow::ensure!(off == 0, "{off} viewers in this process ran off their session's owner's shard");
            anyhow::ensure!(on > on0, "this one ran on its owner's shard: {on0} then {on}");
            Ok(())
        })
    }

    /// A command on a session's socket doesn't wait for the store: with the store's one connection
    /// held (a checkpoint on a busy disk, another caller's long write), each one's audit row is
    /// taken at once and the command goes on, where it once waited for the row to be written first
    /// and a message showed seconds late; and the rows are all written, in order, once the store is
    /// free, the socket having let go of its writer meanwhile.
    #[test]
    fn a_command_on_the_socket_goes_without_waiting_for_the_store() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            use std::time::{Duration, Instant};
            let hub = crate::hub::tests::scratch_hub().await?;
            let (held, holding) = async_channel::bounded::<()>(1);
            let busy = hub.store.clone();
            let hold = crate::rt::spawn(async move {
                busy.call(move |_| {
                    let _ = held.send_blocking(());
                    std::thread::sleep(Duration::from_millis(1500));
                    Ok(())
                })
                .await
            });
            holding.recv().await?;
            // A row written in line, beside the commands: it waits for the store.
            let store = hub.store.clone();
            let in_line = crate::rt::spawn(async move {
                let at = Instant::now();
                store.audit("user:dev1".into(), "probe".into(), "in line".into()).await;
                at.elapsed()
            });
            let mut audit = None;
            let going = Instant::now();
            for n in 1..=3 {
                let send: crate::harness::model::Command = serde_json::from_value(json!({"type": "send", "id": format!("m{n}"), "text": "go"}))?;
                audit_command(&hub, &mut audit, "user:dev1", &format!("gone-{n}"), &send).await;
            }
            let went = going.elapsed();
            drop(audit);
            let waited = in_line.await.map_err(|_| anyhow::anyhow!("the row in line was never written"))?;
            anyhow::ensure!(waited >= Duration::from_millis(500), "the store wasn't busy: a row in line waited {waited:?}");
            anyhow::ensure!(went * 4 < waited, "the commands waited for the store: {went:?} for three, {waited:?} for a row in line");
            let deadline = Instant::now() + Duration::from_secs(10);
            let rows = loop {
                let all = crate::audit::list(&hub.store, &HashMap::new()).await?;
                let mut rows: Vec<(String, String)> = all
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|r| r.get("action").and_then(serde_json::Value::as_str) == Some("agent.command"))
                    .map(|r| (r.get("principal").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned(), r.get("detail").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned()))
                    .collect();
                // Newest first: in the order they were written.
                rows.reverse();
                if rows.len() >= 3 || Instant::now() > deadline {
                    break rows;
                }
                crate::rt::sleep(Duration::from_millis(50)).await;
            };
            let want: Vec<(String, String)> = (1..=3).map(|n| ("user:dev1".to_owned(), format!("gone-{n}: send"))).collect();
            anyhow::ensure!(rows == want, "{rows:?}");
            let _ = hold.await;
            Ok(())
        })
    }

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
    fn the_share_of_runners_back_after_a_start() {
        assert_eq!(back(0, 0), 1.0, "none before: all of them");
        assert_eq!(back(3, 6), 0.5);
        assert_eq!(back(2, 3), 0.667);
        assert_eq!(back(9, 6), 1.0, "more than before is all of them");
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

    /// docs/previews.md, "Sandboxed": a shown page's policy names its own folder and kiln's fonts at
    /// the address it was reached at, beside `'self'`, which WebKit (every browser on iOS) matches
    /// with nothing in the conversation's sandboxed frame: phones were refused a page's own scripts,
    /// pictures and fonts. A `Host` that isn't a name and a port adds nothing to it.
    #[test]
    fn a_page_s_policy_names_its_own_files_where_it_was_reached() {
        let id = "0582dcb7c0e9dbd3a891b242896e61c8";
        let phone = named_sources("Kiln.dev", true, id);
        assert_eq!(phone, format!(" https://kiln.dev/v/{id}/ https://kiln.dev/v/_/fonts/"));
        assert_eq!(named_sources("192.168.1.77:7070", false, id), format!(" http://192.168.1.77:7070/v/{id}/ http://192.168.1.77:7070/v/_/fonts/"));
        for odd in ["", "kiln.dev; script-src *", "kiln.dev https://elsewhere.example", "kiln.dev,evil", "u@kiln.dev", "[fd7a::1]:443"] {
            assert_eq!(named_sources(odd, true, id), "", "{odd:?}");
        }
        let policy = page_policy(&phone, "frame-ancestors 'self'");
        for d in ["default-src", "script-src", "style-src", "img-src", "media-src", "font-src", "connect-src", "worker-src", "frame-src"] {
            assert!(policy.contains(&format!("{d} 'self' https://kiln.dev/v/{id}/ https://kiln.dev/v/_/fonts/ ")), "{d}: {policy}");
        }
        assert!(policy.starts_with("sandbox allow-scripts ") && policy.ends_with("; frame-ancestors 'self'"), "{policy}");
        assert_eq!(page_policy("", "frame-ancestors 'self'"), format!("{PAGE_POLICY}; frame-ancestors 'self'"), "none named: the policy as it was");
    }

    /// A connection that takes each write whole and says it on its channel: what a socket's writes were, as
    /// many as its system calls would be.
    struct Writes(std::sync::mpsc::Sender<Vec<u8>>);

    impl futures::io::AsyncRead for Writes {
        fn poll_read(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>, _: &mut [u8]) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Pending
        }
    }

    impl futures::io::AsyncWrite for Writes {
        fn poll_write(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>, buf: &[u8]) -> std::task::Poll<std::io::Result<usize>> {
            let _ = self.0.send(buf.to_vec());
            std::task::Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_close(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// A session's frame of revision `rev` saying `text`, as its feed puts it on the bus.
    fn fed_frame(offset: u64, text: &str, rev: u64) -> kiln_seams::bus::Message {
        let frame = crate::harness::registry::Frame { text: Arc::from(text), patch: None, patch_of: crate::harness::registry::PatchOf::State, rev };
        kiln_seams::bus::Message { offset, key: None, payload: crate::harness::live::encode(&frame), at_ms: 0 }
    }

    /// The text messages a client reads from `wire`, a server's writes, in order.
    fn texts_read(wire: Vec<u8>) -> anyhow::Result<Vec<String>> {
        let mut client = tungstenite::protocol::WebSocket::from_raw_socket(std::io::Cursor::new(wire), tungstenite::protocol::Role::Client, None);
        let mut texts = Vec::new();
        loop {
            match client.read() {
                Ok(ws::Message::Text(t)) => texts.push(t.as_str().to_owned()),
                Ok(other) => anyhow::bail!("not text: {other:?}"),
                // The writes' end, at a frame's: tungstenite says the stream ended with no close.
                Err(tungstenite::Error::Protocol(tungstenite::error::ProtocolError::ResetWithoutClosingHandshake)) => return Ok(texts),
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Goal 9, plan 01a1a87b2cec step 5: a batch of a session's feed goes to its viewer's socket in one write:
    /// each live frame fed, the batch flushed once, where sending each frame wrote each (a system call on the
    /// shard a frame). A frame the viewer's catch-up held is skipped, the rest arrive whole and in order, the
    /// viewer's revision moves to the newest sent; a batch ending with the session's last frame is one write too,
    /// and says the viewer ends.
    ///
    /// Written off a shard, where no turn ends: on one, a batch longer than its shard's turn is flushed as each of its
    /// turns ends as well (goal 10, plan 01a1a87b2dae step 5), as
    /// `g10_s5_a_feeds_batch_longer_than_a_turn_goes_a_turn_at_a_time` shows; a debug build on a busy host took two
    /// turns for these four frames.
    #[test]
    fn g9_s5_a_feeds_batch_goes_to_its_viewer_in_one_write() -> anyhow::Result<()> {
        /// Shape: the live frames of the batch: more than one, so one write for them all is seen.
        const FRAMES: u64 = 5;
        futures::executor::block_on(async {
            let (said, writes) = std::sync::mpsc::channel::<Vec<u8>>();
            let socket = async_tungstenite::WebSocketStream::from_raw_socket(Writes(said), tungstenite::protocol::Role::Server, None).await;
            let (mut tx, _rx) = socket.split();
            let texts: Vec<String> = (1..=FRAMES).map(|rev| format!(r#"{{"t":"events","rev":{rev}}}"#)).collect();
            // The viewer's catch-up held revision 1: that frame isn't sent again.
            let mut have = 1;
            let batch = (1..=FRAMES).zip(&texts).map(|(rev, text)| fed_frame(rev, text, rev)).collect();
            let came_to = write_live_batch(&mut tx, batch, &mut have, Default::default(), "sessions/s/0.0", Read::now(crate::harness::live::now_us(), true), &mut crate::rt::Turns::new()).await?;
            anyhow::ensure!(came_to == Fed::Written, "{came_to:?}");
            anyhow::ensure!(have == FRAMES, "the viewer has the newest sent: {have}");
            let wrote: Vec<Vec<u8>> = writes.try_iter().collect();
            anyhow::ensure!(wrote.len() == 1, "{} frames of one batch went in {} writes", FRAMES - 1, wrote.len());
            anyhow::ensure!(texts_read(wrote.concat())? == texts.get(1..).unwrap_or_default(), "the frames, whole and in order, the held one skipped");
            // As they went before: a write a frame.
            for text in &texts {
                tx.send(ws::text(text.as_str())).await?;
            }
            anyhow::ensure!(writes.try_iter().count() == texts.len(), "sending each frame writes each");
            // A batch the session's last frame ends: one write, and the viewer ends with it.
            let closed = crate::harness::registry::Frame::of(SessionDown::from(SessionFrame::Closed));
            let last = vec![fed_frame(9, r#"{"t":"events","rev":9}"#, 9), kiln_seams::bus::Message { offset: 10, key: None, payload: crate::harness::live::encode(&closed), at_ms: 0 }];
            let came_to = write_live_batch(&mut tx, last, &mut have, Default::default(), "sessions/s/0.0", Read::now(crate::harness::live::now_us(), true), &mut crate::rt::Turns::new()).await?;
            anyhow::ensure!(came_to == Fed::Closed, "{came_to:?}");
            let wrote: Vec<Vec<u8>> = writes.try_iter().collect();
            anyhow::ensure!(wrote.len() == 1, "a frame and the last went in {} writes", wrote.len());
            anyhow::ensure!(texts_read(wrote.concat())? == [r#"{"t":"events","rev":9}"#.to_owned(), crate::harness::live::closed().to_owned()], "the frame, then the last");
            anyhow::Ok(())
        })
    }

    /// Goal 10, plan 01a1a87b2dae step 5: a batch of a session's feed longer than its shard's turn goes out a turn at a
    /// time: what a turn fed is flushed as the turn ends, and its poll ends there, the rest fed in the turns after (a
    /// viewer's live polls held its shard 0.78 ms on the CPU at the longest in rt9's calm rounds). Each frame here
    /// takes the shard's whole quantum (a `slow` [`Written`]), so each ends its turn: a flush as each turn ends and the
    /// batch's own, the frames whole and in order, and the batch ends as one within a turn does.
    #[test]
    fn g10_s5_a_feeds_batch_longer_than_a_turn_goes_a_turn_at_a_time() -> anyhow::Result<()> {
        /// Shape: the live frames of the batch: more than one, so a turn's end between two is seen.
        const FRAMES: u64 = 3;
        crate::rt::block_on(async {
            let texts: Vec<String> = (1..=FRAMES).map(|rev| format!(r#"{{"t":"events","rev":{rev}}}"#)).collect();
            let batch = (1..=FRAMES).zip(&texts).map(|(rev, text)| fed_frame(rev, text, rev)).collect();
            let (mut tx, mut have) = (Written { slow: true, ..Written::default() }, 0);
            let came_to = write_live_batch(&mut tx, batch, &mut have, Default::default(), "sessions/s/0.0", Read::now(crate::harness::live::now_us(), true), &mut crate::rt::Turns::new()).await?;
            anyhow::ensure!(came_to == Fed::Written && have == FRAMES, "{came_to:?}, the viewer at {have}");
            let sent: Vec<&str> = tx.sent.iter().filter_map(|m| m.to_text().ok()).collect();
            anyhow::ensure!(sent == texts, "the frames, whole and in order: {sent:?}");
            anyhow::ensure!(tx.flushes == texts.len() + 1, "{} flushes: one as each of {} turns ended, and the batch's", tx.flushes, texts.len());
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 3: a batch read at once after another goes on in the turn its viewer's poll
    /// began, not one of its own: a turn already past its quantum ends after the batch's first frame (a flush, and its
    /// poll's end), the rest fed in the next turn. Each batch began its turn afresh, so a poll of several batches went
    /// on past the quantum.
    #[test]
    fn g9_s3_a_batch_goes_on_in_its_viewers_turn() -> anyhow::Result<()> {
        /// Shape: the live frames of the batch: more than one, so the turn's end after the first is seen.
        const FRAMES: u64 = 3;
        crate::rt::block_on(async {
            let quantum = hyper_rt::futures::step_budget_ns().ok_or_else(|| anyhow::anyhow!("not on a shard"))?;
            let texts: Vec<String> = (1..=FRAMES).map(|rev| format!(r#"{{"t":"events","rev":{rev}}}"#)).collect();
            let batch = (1..=FRAMES).zip(&texts).map(|(rev, text)| fed_frame(rev, text, rev)).collect();
            // The batches before it in this poll took its whole quantum.
            let mut turns = crate::rt::Turns::new();
            std::thread::sleep(Duration::from_nanos(quantum));
            let (mut tx, mut have) = (Written::default(), 0);
            let came_to = write_live_batch(&mut tx, batch, &mut have, Default::default(), "sessions/s/0.0", Read::now(crate::harness::live::now_us(), true), &mut turns).await?;
            anyhow::ensure!(came_to == Fed::Written && have == FRAMES, "{came_to:?}, the viewer at {have}");
            let sent: Vec<&str> = tx.sent.iter().filter_map(|m| m.to_text().ok()).collect();
            anyhow::ensure!(sent == texts, "the frames, whole and in order: {sent:?}");
            // A turn after it may end too on a busy host (a debug build's frame is slow): the first flush is the spent turn's.
            anyhow::ensure!(tx.flushed_at.first() == Some(&1) && tx.flushed_at.last() == Some(&texts.len()), "flushed with {:?} sent: the spent turn's after the first frame, the batch's after its last", tx.flushed_at);
            Ok(())
        })
    }

    /// Goal 10, plan 01a1a87b2dae step 5: each live frame a viewer writes is timed by its legs past its owner, to the
    /// microsecond: its wait from its publish for the viewer's task to take it up, as `woken` where the viewer runs on its
    /// session's owner's shard and `woken_other` across shards, and the batch's write, each frame's. A frame the viewer's
    /// catch-up held, not fed, is neither. The histograms are the process's: other tests' frames only add to them.
    #[test]
    fn g10_s5_a_viewers_frames_are_timed_by_their_wake_and_their_write() -> anyhow::Result<()> {
        use crate::route_latency::{FrameLeg, frame_legs};
        /// Shape: the frames a batch feeds, more than one, so each frame's own observation is seen.
        const FED: u64 = 3;
        /// Shape: how long before its viewer's poll each frame was published, past every bucket under it.
        const BEFORE_US: u64 = 700;
        let leg = |l: FrameLeg| frame_legs().of(l).map_or((0, 0), |h| (h.count(), h.sum_us()));
        // A frame published `BEFORE_US` before `woke`: its payload's stamp set so.
        let published = |offset: u64, rev: u64, woke: u64| {
            let mut m = fed_frame(offset, &format!(r#"{{"t":"events","rev":{rev}}}"#), rev);
            if let Some(at) = m.payload.get_mut(..8) {
                at.copy_from_slice(&woke.saturating_sub(BEFORE_US).to_le_bytes());
            }
            m
        };
        crate::rt::block_on(async move {
            for own in [false, true] {
                let taken = if own { FrameLeg::Woken } else { FrameLeg::WokenOther };
                let (woken0, write0) = (leg(taken), leg(FrameLeg::Write));
                // Its viewer woke `BEFORE_US` past now: the process's microsecond clock starts at its first reading, which
                // may be this one.
                let woke = crate::harness::live::now_us().saturating_add(BEFORE_US);
                // Revision 1 the catch-up held: skipped, so neither timed.
                let batch = (1..=FED + 1).map(|rev| published(rev, rev, woke)).collect();
                let (mut tx, mut have) = (Written::default(), 1);
                let came_to = write_live_batch(&mut tx, batch, &mut have, Default::default(), "sessions/s/0.0", Read::now(woke, own), &mut crate::rt::Turns::new()).await?;
                anyhow::ensure!(came_to == Fed::Written && tx.sent.len() == usize::try_from(FED)?, "{came_to:?}, {} sent", tx.sent.len());
                let (woken, write) = (leg(taken), leg(FrameLeg::Write));
                anyhow::ensure!(woken.0 >= woken0.0 + FED && woken.1 >= woken0.1 + FED * BEFORE_US, "{}: {FED} frames each {BEFORE_US} µs: {woken0:?} to {woken:?}", taken.name());
                anyhow::ensure!(write.0 >= write0.0 + FED, "the batch's write, each frame's: {write0:?} to {write:?}");
            }
            Ok(())
        })
    }

    /// Goal 10, plan 01a1a87b2dae step 5: the sessions' list's joins are learned as RFC 6298 learns a round trip: the
    /// first sets the mean to its time and the deviation to half of it, each after moves the mean by an eighth of its
    /// difference and the deviation by a quarter of its distance; the next join is bounded by the mean and four
    /// deviations, and made on the request's shard only once one was learned, and only while that bound fits the shard's
    /// quantum (never off a shard).
    #[test]
    fn g10_s5_the_sessions_lists_joins_are_learned_and_made_where_they_fit() {
        let us = Duration::from_micros;
        let j = Joins::default();
        assert_eq!(j.bound_ns(), None);
        assert!(!j.here(Some(266_667)), "none learned: to the compute pool, which learns it");
        j.took(us(40));
        assert_eq!((j.mean_ns.load(std::sync::atomic::Ordering::Relaxed), j.dev_ns.load(std::sync::atomic::Ordering::Relaxed)), (40_000, 20_000), "R, and R/2");
        assert_eq!(j.bound_ns(), Some(120_000), "R + 4 R/2");
        assert!(j.here(Some(266_667)) && !j.here(Some(100_000)) && !j.here(None), "within the quantum: here; past it, or off a shard: not");
        j.took(us(120));
        // RTTVAR = 3/4 · 20 + 1/4 · |40 − 120| = 35 µs; SRTT = 7/8 · 40 + 1/8 · 120 = 50 µs.
        assert_eq!((j.mean_ns.load(std::sync::atomic::Ordering::Relaxed), j.dev_ns.load(std::sync::atomic::Ordering::Relaxed)), (50_000, 35_000));
        assert_eq!(j.bound_ns(), Some(190_000));
        // A list grown past the quantum's join goes off the shard, and comes back once its joins are short again.
        for _ in 0..8 {
            j.took(us(900));
        }
        assert!(!j.here(Some(266_667)), "{:?}", j.bound_ns());
        for _ in 0..64 {
            j.took(us(30));
        }
        assert!(j.here(Some(266_667)), "{:?}", j.bound_ns());
    }

    /// Goal 9, plan 01a1a87b2cec step 3: [`waited`] says whether what it awaited made its task wait (a poll of it was
    /// pending), so a viewer's batch that came after a wait begins a turn of its own, and one ready at once goes on in
    /// its poll's.
    #[test]
    fn g9_s3_waited_says_whether_its_task_waited() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let (out, pending) = waited(async { 7 }).await;
            anyhow::ensure!(out == 7 && !pending, "ready at once: {out}, waited {pending}");
            let (out, pending) = waited(async {
                hyper_rt::futures::yield_now().await;
                8
            })
            .await;
            anyhow::ensure!(out == 8 && pending, "after a yield: {out}, waited {pending}");
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 3: the reads kept ahead (`GET /api/agents`, `/api/overview`, the owner's `/api/me`)
    /// are answered named by their handlers, with the weak ETag made once with them (`crate::reads::Made`): the very
    /// one `crate::http::compress::finish` made of their bodies at each read before. Through the API the validator is
    /// as it was, and a revalidation with it is answered 304.
    #[test]
    fn g9_s3_reads_kept_ahead_are_named_as_finish_named_them() -> anyhow::Result<()> {
        use crate::http::server::Handler as _;
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let api = Arc::new(Api { hub: Arc::clone(&hub) });
            let owner = crate::authz::Caller::owner();
            // The first reads make what's kept (the maker too, beside them); the uptime the overview says may turn a
            // second between two reads: read again.
            let deadline = Instant::now() + Duration::from_secs(30);
            for path in ["/api/agents", "/api/overview", "/api/me"] {
                loop {
                    let answered = match path {
                        "/api/agents" => agents_read(&hub, &owner).await,
                        "/api/overview" => overview(&hub, &owner).await.map_err(|e| anyhow::anyhow!("{} {}", e.0, e.1))?,
                        _ => me(&hub, lab_get(path)?.headers()).await,
                    };
                    let named = answered.headers().get(header::ETAG).cloned();
                    let body = body_of(answered).await?;
                    if let Some(named) = named {
                        anyhow::ensure!(named.to_str()? == crate::http::compress::etag_of(&body), "{path}: named {named:?}, where its body's digest names it otherwise");
                        let r = Arc::clone(&api).handle(lab_get(path)?).await;
                        let etag = r.headers().get(header::ETAG).cloned();
                        if body_of(r).await? == body {
                            anyhow::ensure!(etag.as_ref() == Some(&named), "{path}: validated {etag:?} through the API");
                            let mut asked = lab_get(path)?;
                            asked.headers_mut().insert(header::IF_NONE_MATCH, named.clone());
                            if Arc::clone(&api).handle(asked).await.status() == StatusCode::NOT_MODIFIED {
                                break;
                            }
                        }
                    }
                    anyhow::ensure!(Instant::now() < deadline, "{path} wasn't answered named, nor revalidated");
                    crate::rt::sleep(Duration::from_millis(5)).await;
                }
            }
            Ok(())
        })
    }

    /// Goal 10, plan 01a1a87b2dae step 5: a request's future holds its router's state by a pointer, not a copy
    /// of it in each layer it's carried in (its label, its placement): the future kiln-http keeps for each
    /// request, boxed anew when its connection idled between requests, is smaller than the router's. In this
    /// (test) build it was 54,816 bytes, three times a router's 8,808; now 2,064, the router's 2,840 boxed
    /// apart once (its inline awaits of Observe's boxed too).
    #[test]
    fn g10_s5_a_requests_future_holds_its_router_by_pointer() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let api = Arc::new(Api { hub: Arc::clone(&hub) });
            let router = route(Arc::clone(&hub), lab_get("/api/me")?);
            let handled = crate::http::server::Handler::handle(Arc::clone(&api), lab_get("/api/me")?);
            let placed = crate::placement::request(crate::overload::Class::Normal, false, crate::http::server::Handler::handle(Arc::clone(&api), lab_get("/api/me")?));
            let (router_b, handled_b, placed_b) = (size_of_val(&router), size_of_val(&handled), size_of_val(&placed));
            drop((router, handled, placed));
            anyhow::ensure!(handled_b < router_b && placed_b < router_b, "the router's {router_b} bytes, a request's {handled_b}, placed {placed_b}");
            Ok(())
        })
    }

    /// Goal 10, plan 01a1a87b2dae step 5: a request that asks (`kiln-timing`) is told its own wait before its handler
    /// started, from its head's arrival, and its handler's time, as Server-Timing, in milliseconds to the
    /// microsecond; one that doesn't ask is answered as before, without it.
    #[test]
    fn g10_s5_a_request_that_asks_is_told_its_own_wait_and_took() -> anyhow::Result<()> {
        use crate::http::server::Handler as _;
        assert_eq!(with_timing(srv::status(StatusCode::OK), Some(Duration::from_micros(1_234)), Duration::from_micros(56)).headers().get("server-timing").map(|v| v.as_bytes().to_vec()), Some(b"wait;dur=1.234, took;dur=0.056".to_vec()));
        assert_eq!(with_timing(srv::status(StatusCode::OK), None, Duration::from_micros(56)).headers().get("server-timing").map(|v| v.as_bytes().to_vec()), Some(b"took;dur=0.056".to_vec()));
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let api = Arc::new(Api { hub: Arc::clone(&hub) });
            let dur = |v: &str, name: &str| v.split(',').find_map(|m| m.trim().strip_prefix(name)?.strip_prefix(";dur=")?.parse::<f64>().ok());
            // Its head came 2 ms before its handler started.
            let mut asks = lab_get("/api/me")?;
            asks.headers_mut().insert(ASK_TIMING, header::HeaderValue::from_static("1"));
            asks.extensions_mut().insert(kiln_http::Received(Instant::now().checked_sub(Duration::from_millis(2)).unwrap_or_else(Instant::now)));
            let resp = Arc::clone(&api).handle(asks).await;
            anyhow::ensure!(resp.status() == StatusCode::OK, "{}", resp.status());
            let said = resp.headers().get("server-timing").and_then(|v| v.to_str().ok()).map(str::to_owned).unwrap_or_default();
            let (wait, took) = (dur(&said, "wait"), dur(&said, "took"));
            anyhow::ensure!(wait.is_some_and(|w| w >= 2.0) && took.is_some_and(|t| t >= 0.0), "{said:?}");
            // As it was for a request that doesn't ask.
            let resp = Arc::clone(&api).handle(lab_get("/api/me")?).await;
            anyhow::ensure!(resp.status() == StatusCode::OK && !resp.headers().contains_key("server-timing"), "{:?}", resp.headers());
            Ok(())
        })
    }

    /// Goal 10, plan 01a1a87b2dae step 5: the server's figures are written out off the request's shard and answered
    /// whole, every family there (the routes', the runner's legs with the loop's hold); without the owner's token,
    /// refused as before.
    #[test]
    fn g10_s5_the_figures_are_written_out_off_the_shard_and_answered_whole() -> anyhow::Result<()> {
        use crate::http::server::Handler as _;
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let api = Arc::new(Api { hub: Arc::clone(&hub) });
            // The legs' series are made by the first runner's message the hub times (goal 11, plan 01a1a87b2e56 step 5:
            // a runner's process renders none), which this hub hasn't had.
            let _ = crate::route_latency::legs();
            let resp = Arc::clone(&api).handle(lab_get("/metrics")?).await;
            anyhow::ensure!(resp.status() == StatusCode::OK, "{}", resp.status());
            let body = http_body_util::BodyExt::collect(resp.into_body()).await.map_err(|e| anyhow::anyhow!("{e}"))?.to_bytes();
            let text = String::from_utf8_lossy(&body);
            for family in ["kiln_route_seconds", "kiln_runner_wait_leg_seconds_count{leg=\"held\"}", "kiln_memory_bytes"] {
                anyhow::ensure!(text.contains(family), "{family} missing from {} bytes", text.len());
            }
            if !hub.cfg.server.metrics_public {
                let anyone = http::Request::builder().method(Method::GET).uri("/metrics").body(srv::Incoming::empty())?;
                let resp = Arc::clone(&api).handle(anyone).await;
                anyhow::ensure!(resp.status() == StatusCode::UNAUTHORIZED, "{}", resp.status());
            }
            Ok(())
        })
    }

    /// Goal 10, plan 01a1a87b2dae step 5: a path's segments, split once by the router, are each the path's
    /// own bytes where there's nothing to decode in it (no copy made), decoded where there is.
    #[test]
    fn g10_s5_a_paths_segments_are_its_own_unless_decoded() {
        let path = "/api/harness/sessions/a%2Fb/events/two+words/";
        let seg = segments(path);
        let said: Vec<&str> = seg.iter().map(|c| &**c).collect();
        assert_eq!(said, ["api", "harness", "sessions", "a/b", "events", "two words"]);
        for (c, plain) in seg.iter().zip([true, true, true, false, true, false]) {
            assert_eq!(matches!(c, Cow::Borrowed(_)), plain, "{c}");
            if let Cow::Borrowed(s) = c {
                assert!(path.as_bytes().as_ptr_range().contains(&s.as_ptr()), "{s}: the path's own bytes");
            }
        }
        assert!(segments("/").is_empty());
    }

    /// What `f` allocated on this thread, and its output.
    fn allocating<T>(f: impl FnOnce() -> T) -> (T, u64) {
        let before = crate::os::counted::made().blocks;
        let out = f();
        (out, crate::os::counted::made().blocks.saturating_sub(before))
    }

    /// Goal 9, plan 01a1a87b2cec step 5: the router's segments are matched from the stack, as many as a route names,
    /// with no allocation; a path of more has them in a vector.
    #[test]
    fn g9_s5_the_routers_segments_are_matched_from_the_stack() {
        let seg = segments("/api/harness/sessions/a%2Fb/events");
        let mut room = [""; srv::ROUTE_DEPTH];
        let (said, made) = allocating(|| match strs(&seg, &mut room) {
            Cow::Borrowed(s) => matches!(s, ["api", "harness", "sessions", "a/b", "events"]),
            Cow::Owned(_) => false,
        });
        assert!(said && made == 0, "matched from the stack: {made} allocations");
        let seg = segments("/api/harness/sessions/a/files/src/kiln/lib/http/server.rs");
        let mut room = [""; srv::ROUTE_DEPTH];
        let s = strs(&seg, &mut room);
        assert!(matches!(s, Cow::Owned(_)));
        assert_eq!(s.join("/"), "api/harness/sessions/a/files/src/kiln/lib/http/server.rs");
    }

    /// Goal 9, plan 01a1a87b2cec step 5 (authorization's work a request): a session's cookie is checked with nothing
    /// allocated (it was a copy of the cookie, the string signed and the signature's hex), and says what the
    /// signature's own comparison says: the owner's own, yes; one whose signature or expiry was changed, or one past
    /// its expiry, no.
    #[test]
    fn g9_s5_a_sessions_cookie_is_checked_without_allocating() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let value = signed_expiring(&hub, "session", SESSION_TTL);
            let with = |v: &str| -> anyhow::Result<HeaderMap> {
                let mut h = HeaderMap::new();
                h.insert(header::COOKIE, header::HeaderValue::from_str(&format!("theme=dark; {SESSION_COOKIE}={v}"))?);
                Ok(h)
            };
            let theirs = with(&value)?;
            let (yes, made) = allocating(|| authed(&hub, &theirs));
            anyhow::ensure!(yes && made == 0, "the owner's session: {yes}, {made} allocations");
            let (exp, sig) = value.split_once('.').ok_or_else(|| anyhow::anyhow!("no signature in {value}"))?;
            anyhow::ensure!(crate::util::ct_eq(sig.as_bytes(), sign(&hub, "session", exp).as_bytes()), "the signature as it's made");
            let flipped = if sig.ends_with('0') { "1" } else { "0" };
            let changed = format!("{exp}.{}{flipped}", sig.get(..sig.len().saturating_sub(1)).unwrap_or_default());
            let later = format!("{}.{sig}", exp.parse::<i64>()? + 1);
            let past = format!("{}.{}", crate::util::now_unix() - 1, sign(&hub, "session", &(crate::util::now_unix() - 1).to_string()));
            for (v, why) in [(changed, "its signature changed"), (later, "its expiry changed"), (past, "past its expiry")] {
                anyhow::ensure!(!authed(&hub, &with(&v)?), "{why}: refused");
                anyhow::ensure!(!verify_expiring(&hub, "session", &v), "{why}: refused");
            }
            anyhow::ensure!(!verify_expiring(&hub, "otp", &value), "another purpose's");
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 5: a request's own timing is said with one allocation, its header's own, where a
    /// string was made for it first; the words as before.
    #[test]
    fn g9_s5_a_requests_timing_is_said_in_one_allocation() {
        let resp = srv::text(StatusCode::OK, "ok");
        let (resp, made) = allocating(|| with_timing(resp, Some(Duration::from_micros(1500)), Duration::from_micros(250)));
        assert_eq!(made, 1);
        assert_eq!(resp.headers().get("server-timing").and_then(|v| v.to_str().ok()), Some("wait;dur=1.500, took;dur=0.250"));
        let resp = with_timing(srv::text(StatusCode::OK, "ok"), None, Duration::from_secs(u64::MAX));
        assert!(resp.headers().get("server-timing").and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with("took;dur=18446744073709551")), "the longest fits");
    }

    /// What each request cost the server: the allocations its connection's polls made, their bytes, and the polls'
    /// time, by path, the median of its repeats.
    type Costs = Vec<(&'static str, crate::os::counted::Made, Duration)>;

    /// One kept-alive connection to kiln's API on a port of its own, over HTTP/1.1 or HTTP/2, the server's side of it
    /// counted (`crate::os::counted`): what each of `paths`, asked `reps` times as the lab's readers ask it (the
    /// owner's token, nothing packed, its own timing asked), cost the server once the connection had gone idle after
    /// the request before, as theirs does (goal 9, plan 01a1a87b2cec step 5).
    async fn served(api: Arc<Api>, h2: bool, paths: &[&'static str], reps: usize) -> anyhow::Result<Costs> {
        use crate::os::counted::{Made, Tally, counted_into};
        use http_body_util::{BodyExt as _, Empty};
        let tcp = hyper_rt::tcp::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), 16).map_err(|e| anyhow::anyhow!("{e}"))?;
        let addr = tcp.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
        let tally = Arc::new(Tally::default());
        let counting = Arc::clone(&tally);
        crate::rt::spawn(async move {
            if let Ok(stream) = tcp.accept().await {
                let remote = stream.peer_addr().ok();
                counted_into(&counting, crate::http::server::serve_accepted(stream, api, remote, None)).await;
            }
        });
        let io = crate::http::io::TcpIo::new(hyper_rt::tcp::TcpStream::connect(addr).await.map_err(|e| anyhow::anyhow!("{e}"))?);
        enum Client {
            One(kiln_http::client::http1::SendRequest<Empty<Bytes>>),
            Two(kiln_http::client::http2::SendRequest<Empty<Bytes>>),
        }
        let mut client = if h2 {
            let (send, conn) = kiln_http::client::http2::handshake::<_, Empty<Bytes>>(io).await?;
            crate::rt::spawn(conn);
            Client::Two(send)
        } else {
            let (send, conn) = kiln_http::client::http1::handshake::<_, Empty<Bytes>>(io).await?;
            crate::rt::spawn(conn);
            Client::One(send)
        };
        let mut costs = Vec::new();
        for path in paths {
            let mut each = Vec::new();
            for _ in 0..reps {
                let uri = if h2 { format!("http://127.0.0.1:{}{path}", addr.port()) } else { (*path).to_owned() };
                let req = http::Request::builder()
                    .uri(uri)
                    .header(header::HOST, format!("127.0.0.1:{}", addr.port()))
                    .header(header::AUTHORIZATION, "Bearer test-admin-token")
                    .header(header::ACCEPT, "*/*")
                    .header(header::ACCEPT_ENCODING, "identity")
                    .header(ASK_TIMING, "1")
                    .body(Empty::<Bytes>::new())?;
                let before = tally.now();
                let resp = match &mut client {
                    Client::One(send) => send.send_request(req).await?,
                    Client::Two(send) => send.send_request(req).await?,
                };
                let status = resp.status();
                let body = resp.into_body().collect().await?.to_bytes();
                anyhow::ensure!(status == StatusCode::OK, "{path}: {status} {}", String::from_utf8_lossy(&body));
                // The server's polls for it ended: its connection is idle again.
                crate::rt::sleep(Duration::from_millis(3)).await;
                let after = tally.now();
                each.push((after.0.blocks.saturating_sub(before.0.blocks), after.0.bytes.saturating_sub(before.0.bytes), after.1.saturating_sub(before.1)));
            }
            let mid = |mut v: Vec<u64>| {
                v.sort_unstable();
                v.get(v.len() / 2).copied().unwrap_or_default()
            };
            let made = Made { blocks: mid(each.iter().map(|e| e.0).collect()), bytes: mid(each.iter().map(|e| e.1).collect()) };
            costs.push((*path, made, Duration::from_nanos(mid(each.iter().map(|e| e.2).collect()))));
        }
        Ok(costs)
    }

    /// Goal 9, plan 01a1a87b2cec step 5 (a tenth of the CPU): what serving each read the lab makes of an agent's
    /// pages costs the server, over HTTP/1.1 and HTTP/2, once what's kept for them is made: the allocations a request
    /// makes on its connection's polls (request, router, handler, answer and write, for the median of its repeats),
    /// their bytes and the polls' time, said for the hand-over and held under what the serve path's cuts left.
    #[test]
    fn g9_s5_what_a_read_costs_the_server_route_by_route() -> anyhow::Result<()> {
        use crate::harness::core::Update;
        use crate::harness::model::{Body, Event, State};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = crate::harness::registry::Location::Machine("box".into());
            let info = crate::harness::session::Info { id: "a".into(), kind: "claude".into(), ..crate::harness::session::Info::default() };
            hub.harness.ingest(&hub, &there, "a", Update::Open { info: Box::new(info), since: None }).await;
            let said = |n: u64| Event { id: format!("e{n}"), seq: n, at: 1, parent: None, by: None, body: Body::Assistant { text: format!("{n}: the deadline"), streaming: false } };
            hub.harness.ingest(&hub, &there, "a", Update::Events { events: (1..=60).map(said).collect() }).await;
            hub.harness.ingest(&hub, &there, "a", Update::State { state: Box::new(State::default()) }).await;
            let source = crate::plans::Source { session: Some("a".into()), ..crate::plans::Source::default() };
            crate::plans::create(&hub, crate::plans::New { body: "1. Measure the reads\n2. Prove it".into(), source: Some(source), ..crate::plans::New::default() }, "admin").await?;
            crate::goals::add(&hub, "a", vec!["reads from memory".into()], false).await?;
            let api = Arc::new(Api { hub: Arc::clone(&hub) });
            let paths = ["/healthz", "/api/me", "/api/harness/sessions", "/api/harness/sessions/a", "/api/harness/sessions/a/events?limit=50", "/api/harness/sessions/a/goals", "/api/plans?session=a"];
            // What's kept for these reads made (the first reads of each make it, off the request path).
            for h2 in [false, true] {
                served(Arc::clone(&api), h2, &paths, 2).await?;
            }
            crate::rt::sleep(Duration::from_millis(200)).await;
            let one = served(Arc::clone(&api), false, &paths, 15).await?;
            let two = served(Arc::clone(&api), true, &paths, 15).await?;
            for ((path, a, t), (_, b, u)) in one.iter().zip(&two) {
                eprintln!("g9_s5 {path}: HTTP/1.1 {} allocations, {} bytes, {:.1} us; HTTP/2 {} allocations, {} bytes, {:.1} us", a.blocks, a.bytes, t.as_secs_f64() * 1e6, b.blocks, b.bytes, u.as_secs_f64() * 1e6);
            }
            // The serve path's own, under the least a handler does (`/healthz`'s "ok"): 26 allocations over HTTP/1.1
            // and 22 over HTTP/2 before step 5's cuts of it, 16 and 13 after (a request's headers in a map an answer
            // gave back, its arrival one extension, its path matched from the stack, its timing said in place, its
            // answer's body unboxed again; HTTP/2's frame head in place). What's left: kiln-http's read buffer and its
            // share, the request's extensions (their table, kiln-http's arrival time, the connection's), the slot an
            // idle connection makes again, the handler's and the router's futures, the path's segments, the answer's
            // body and its box, its header map and its timing. Two more allow for another test's request on the same
            // shard taking the map given back between two of these. And FINISH more since train 9 put main's compression
            // (W21-R15, `crate::http::compress::finish`) on this path: a GET answered 200 with a body it may validate,
            // /healthz's "ok" too, was 9 on both protocols, measured on main b6519ca5756 and on train 10 alike (25 and
            // 22, 2026-11-06): its future boxed, its type lowered into a copy twice (to tell whether it compresses), a
            // weak ETag (its digest as hex, then as `W/"…"`, then a header value's copy) and the body boxed again. Goal 9
            // step 5 finishes it where it is (no future), tells its type in place and writes its ETag once into the
            // buffer its header value takes: 2 left, the ETag's and the body's box
            // (`crate::http::compress::tests::g9_s5_a_small_answer_is_finished_here_in_two_allocations`).
            const FINISH: u64 = 2;
            let fixed = |costs: &Costs| costs.iter().find(|(p, _, _)| *p == "/healthz").map_or(u64::MAX, |(_, m, _)| m.blocks);
            anyhow::ensure!(fixed(&one) <= 16 + FINISH + 2 && fixed(&two) <= 13 + FINISH + 2, "the serve path's own: {} allocations over HTTP/1.1, {} over HTTP/2", fixed(&one), fixed(&two));
            Ok(())
        })
    }
}
