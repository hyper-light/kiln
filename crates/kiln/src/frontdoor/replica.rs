//! A runner's front door while it holds a replica of kiln's records (docs/p2p.md, P2): requests
//! are served one by one. The owner's reads of what the replica holds ([`crate::reads`]) are
//! answered here, once the replica holds every write the server had made when asked; everything
//! else goes to the server as it came (a read on a connection kept from an earlier request when
//! one is free), a WebSocket piped through.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::{FutureExt as _, StreamExt as _};
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use http_body::Body as _;
use http_body_util::BodyExt as _;
use kiln_http::Incoming;
use kiln_http::client::http1::SendRequest;
use serde_json::json;

use super::proxy::{Listening, MAX_CONNECTIONS, Upstream};
use crate::http::io::{RtFuturesIo, RtIo};
use crate::http::server::{self as srv, Request, Response};
use crate::http::tls::{ClientTls, TlsStream};
use crate::rt::Shutdown;
use crate::store::learner::Learner;

/// How long a read waits for the replica to hold the server's writes; past it, the server answers.
const CURRENT_WITHIN: Duration = Duration::from_secs(2);
/// How long the server's word on whose credentials are the owner's is kept.
const OWNER_FOR: Duration = Duration::from_secs(30);
/// Credentials remembered at once, at most.
const OWNERS: usize = 1024;
/// After the replica didn't catch up in time, how long the server answers every read at once
/// (a replica that no longer follows costs a read nothing).
const STALE_FOR: Duration = Duration::from_secs(30);
/// Headers about one connection, never forwarded either way.
const HOP: [&str; 8] = ["connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te", "trailer", "transfer-encoding", "upgrade"];
/// What an answer from the replica says: the runner, and the last write its replica held.
pub const READ_HEADER: &str = "kiln-read";
/// Present when a read fell back to the local replica while the upstream server was unreachable.
pub const STALE_HEADER: &str = "kiln-read-stale";
/// Header saying how a P2P harness command was routed (`local <machine>` or `quic <peer>`).
pub const ROUTED_HEADER: &str = "kiln-routed";
/// Optional request header carrying the target runner id attached by the UI.
pub const TARGET_HEADER: &str = "x-kiln-target";
/// Maximum request body buffered when inspecting a harness command for local/QUIC routing.
const COMMAND_BODY_MAX: usize = 16 << 20;
/// Timeout waiting for a local session's inbox receipt after local enqueue.
const LOCAL_INBOX_WAIT: Duration = Duration::from_secs(8);
/// Connections to the server kept for the requests after, at most. A page's pictures and frames
/// go on connections earlier requests opened, as they do when a browser's connection is piped
/// whole: one new connection each, on a busy server that takes them late, lost a phone most of
/// them (502s) while kiln's own requests, retried, got through.
const IDLE: usize = 16;
/// Tries at a new connection to the server before a request is answered 502.
const DIALS: u32 = 3;

/// Waiters for local session inbox receipts (`Update::Inbox { said }`), shared between the runner's
/// harness loop and its local HTTP / `hyper-quic` P2P forwarding entrypoints.
#[derive(Default)]
pub struct InboxWaiters {
    pending: parking_lot::Mutex<HashMap<(String, String), Vec<futures::channel::oneshot::Sender<crate::harness::inbox::Said>>>>,
}

impl InboxWaiters {
    /// Register a one-shot waiter for `(session, message)`.
    pub fn register(&self, session: &str, message: &str) -> futures::channel::oneshot::Receiver<crate::harness::inbox::Said> {
        let (tx, rx) = futures::channel::oneshot::channel();
        self.pending.lock().entry((session.to_owned(), message.to_owned())).or_default().push(tx);
        rx
    }

    /// Resolve all waiters registered for `(session, said.message)`.
    pub fn resolve(&self, session: &str, said: &crate::harness::inbox::Said) {
        let waiters = self.pending.lock().remove(&(session.to_owned(), said.message.clone()));
        if let Some(list) = waiters {
            for tx in list {
                let _ = tx.send(said.clone());
            }
        }
    }

    /// Remove any waiter for `(session, message)` on timeout or send failure.
    pub fn cancel(&self, session: &str, message: &str) {
        self.pending.lock().remove(&(session.to_owned(), message.to_owned()));
    }
}

/// Local runner capabilities attached to a node's HTTP entrypoint (`localhost:7070` and HTTPS front door).
#[derive(Clone, Default)]
pub struct LocalNode {
    /// This runner's identity / machine key (matches `AgentInfo::target` without `agent:` prefix).
    pub machine: String,
    /// Optional human-readable label / hostname alias.
    pub label: Option<String>,
    /// Local harness session host on this runner.
    pub host: Option<Arc<crate::harness::host::Host>>,
    /// Waiters resolved when a local session emits `Update::Inbox { said }`.
    pub waiters: Arc<InboxWaiters>,
    /// Reach handle (direct `hyper-quic` first, relayed reach link fallback) for P2P forwarding.
    pub reach: Option<crate::reach::node::Handle>,
    /// Local conversation store replicated via per-session Raft.
    pub convo: Option<crate::convo::store::Store>,
    /// Per-session conversation Raft manager on this node.
    pub convo_raft: Option<Arc<crate::convo::raft::Manager>>,
}

impl LocalNode {
    /// Whether `target` (with or without leading `agent:`) names THIS runner node.
    pub fn matches_target(&self, target: &str) -> bool {
        let Some(t) = normalize_target(target) else { return false };
        (!self.machine.is_empty() && t == self.machine) || self.label.as_deref().is_some_and(|l| !l.is_empty() && t == l)
    }

    /// Enqueue a routed message directly into this runner's local session inbox (ONLY when this
    /// runner actually holds `session` locally; returns `NoSession` without enqueueing otherwise).
    pub async fn enqueue_local(&self, session: &str, epoch: Option<u64>, c: crate::harness::model::Command, route: crate::harness::route::Route) -> crate::reach::wire::P2pSendReply {
        use crate::reach::wire::P2pSendReply;
        let Some(host) = &self.host else { return P2pSendReply::NoSession };
        let Some(msg_id) = crate::harness::registry::message_id(&c) else {
            return P2pSendReply::Error { why: "message has no id".into() };
        };
        if host.get(session).is_none() {
            return P2pSendReply::NoSession;
        }
        let rx = self.waiters.register(session, &msg_id);
        let eff_epoch = epoch.or_else(|| host.get(session).map(|s| s.info().epoch));
        if !host.command_routed(session, c, eff_epoch, route) {
            self.waiters.cancel(session, &msg_id);
            return P2pSendReply::NoSession;
        }
        match crate::rt::timeout(LOCAL_INBOX_WAIT, rx).await {
            Ok(Ok(said)) => match said.answer {
                Ok(answer) => P2pSendReply::Accepted { answer },
                Err(refusal) => P2pSendReply::Refused { refusal },
            },
            Ok(Err(_)) => P2pSendReply::Error { why: "local session inbox dropped waiter".into() },
            Err(_) => {
                self.waiters.cancel(session, &msg_id);
                P2pSendReply::Error { why: "local session inbox timed out".into() }
            }
        }
    }
}

static GLOBAL_LOCAL: arc_swap::ArcSwapOption<LocalNode> = arc_swap::ArcSwapOption::const_empty();

/// Publish this runner's [`LocalNode`] so both `localhost:7070` and the HTTPS front door share it.
pub fn set_local_node(local: Option<LocalNode>) {
    GLOBAL_LOCAL.store(local.map(Arc::new));
}

/// Active [`LocalNode`] on this runner process, if any.
pub fn local_node() -> Option<Arc<LocalNode>> {
    GLOBAL_LOCAL.load_full()
}

/// Normalize a UI/API target string (`"agent:laptop-b"` -> `Some("laptop-b")`, `"local"` / `""` -> `None`).
pub fn normalize_target(raw: &str) -> Option<&str> {
    let t = raw.trim();
    let t = t.strip_prefix("agent:").unwrap_or(t).trim();
    if t.is_empty() || t == "local" { None } else { Some(t) }
}

/// Result of attempting direct local / `hyper-quic` P2P routing on the initial recipient node.
enum P2pRoute {
    LocalAccepted(crate::harness::inbox::Answer),
    LocalRefused(crate::harness::inbox::Refusal),
    QuicAccepted { peer: String, answer: crate::harness::inbox::Answer },
    QuicRefused { peer: String, refusal: crate::harness::inbox::Refusal },
}

/// The front door's handler with a replica and/or local P2P runner node here.
pub struct Door {
    upstream: Upstream,
    /// For a plain-HTTP upstream, which needs none.
    plain: Arc<ClientTls>,
    /// Explicit cluster Raft learner (when constructed via [`Door::new`]); when `None`, resolved
    /// dynamically via [`crate::store::learner::global`] so `localhost:7070` starts immediately.
    learner: Option<Arc<Learner>>,
    machine: String,
    /// Local runner node state for unified `localhost:7070` entrypoint + direct `hyper-quic` forwarding.
    local: Option<LocalNode>,
    /// Which credentials are the owner's, as the server said, and when: a copy read without a lock on
    /// every read, swapped in whole as the server answers (goal 5, plan 01a1a59c2aae step 2: no lock
    /// the front door's shards share; P2, plan 01a1a283e14c).
    owners: crate::rcu::Rcu<HashMap<String, (bool, Instant)>>,
    /// When it was made: what `stale_until` counts from.
    born: Instant,
    /// Until when the server answers every read (the replica didn't catch up the last time), in
    /// nanoseconds after `born`; none while 0.
    stale_until: std::sync::atomic::AtomicU64,
    /// Connections to the server, kept for the next requests ([`IDLE`] at most): a lock-free queue,
    /// put in here and taken from `idle_out`.
    idle: async_channel::Sender<SendRequest<srv::Body>>,
    idle_out: async_channel::Receiver<SendRequest<srv::Body>>,
}

impl Door {
    pub fn new(upstream: Upstream, learner: Arc<Learner>) -> anyhow::Result<Self> {
        let plain = Arc::new(ClientTls::new(crate::http::client::Roots::WebPki, None)?);
        let machine = learner.machine().to_owned();
        let local = local_node().map(|l| (*l).clone());
        // At most IDLE kept: one past it is let go, as it was.
        let (idle, idle_out) = async_channel::bounded(IDLE);
        Ok(Self { upstream, plain, learner: Some(learner), machine, local, owners: crate::rcu::Rcu::default(), born: Instant::now(), stale_until: std::sync::atomic::AtomicU64::new(0), idle, idle_out })
    }

    /// Construct a `Door` for a runner's own HTTP server (`localhost:7070`): starts immediately at
    /// boot, dynamically uses [`crate::store::learner::global`] once cluster Raft joins, serves
    /// `/healthz`, `/healthz/server`, `/api/convo/raft`, and performs direct local / `hyper-quic`
    /// message routing without enqueueing non-local messages on this node.
    pub fn for_node(upstream: Upstream, local: LocalNode) -> anyhow::Result<Self> {
        let plain = Arc::new(ClientTls::new(crate::http::client::Roots::WebPki, None)?);
        let machine = local.machine.clone();
        let (idle, idle_out) = async_channel::bounded(IDLE);
        Ok(Self { upstream, plain, learner: None, machine, local: Some(local), owners: crate::rcu::Rcu::default(), born: Instant::now(), stale_until: std::sync::atomic::AtomicU64::new(0), idle, idle_out })
    }

    /// Active cluster Raft learner for this door (explicit or global).
    fn active_learner(&self) -> Option<Arc<Learner>> {
        self.learner.clone().or_else(crate::store::learner::global)
    }

    /// Whether it's the handler for this replica and this upstream.
    pub fn serves(&self, learner: &Arc<Learner>, upstream: &Upstream) -> bool {
        self.learner.as_ref().is_some_and(|l| Arc::ptr_eq(l, learner)) && self.upstream.url == upstream.url
    }

    /// `after` from now, in nanoseconds after `born`.
    fn since_born(&self, after: Duration) -> u64 {
        u64::try_from(self.born.elapsed().saturating_add(after).as_nanos()).unwrap_or(u64::MAX)
    }

    /// The server answers every read for now: the replica didn't catch up the last time.
    fn stale(&self) -> bool {
        let until = self.stale_until.load(std::sync::atomic::Ordering::Acquire);
        until != 0 && self.since_born(Duration::ZERO) < until
    }

    fn tls(&self) -> &ClientTls {
        self.upstream.tls.as_deref().unwrap_or(&self.plain)
    }

    /// The replica's answer to the owner's read of a route it answers, once it holds every write
    /// the server had made; `None`: the server answers.
    async fn read(&self, req: &Request) -> Option<Response> {
        if req.method() != Method::GET || crate::http::ws::is_upgrade(req) {
            return None;
        }
        let seg: Vec<String> = req.uri().path().trim_matches('/').split('/').filter(|s| !s.is_empty()).map(srv::decode).collect();
        let s: Vec<&str> = seg.iter().map(String::as_str).collect();
        // Node-local health and per-session conversation Raft status endpoints on `localhost:7070`.
        if let Some(local) = &self.local {
            match s.as_slice() {
                ["healthz"] => return Some(srv::text(StatusCode::OK, "ok")),
                ["healthz", "server"] => return Some(self.node_health(local)),
                ["api", "convo", "raft"] => return Some(self.convo_raft_status(local)),
                _ => {}
            }
        }
        let learner = self.active_learner()?;
        if !crate::reads::answers(&s) || self.stale() || !self.owner(req.headers()).await {
            return None;
        }
        if !learner.current(CURRENT_WITHIN).await {
            self.stale_until.store(self.since_born(STALE_FOR), std::sync::atomic::Ordering::Release);
            return None;
        }
        let v = crate::reads::answer(learner.store(), &s, &srv::query(req)).await?;
        let r = srv::json(StatusCode::OK, &v);
        Some(srv::with_header(r, HeaderName::from_static(READ_HEADER), &format!("{} {}", self.machine, learner.standing().0)))
    }

    fn node_health(&self, local: &LocalNode) -> Response {
        let store = self.active_learner().map_or(json!({ "group": false }), |l| {
            let (seq, applied) = l.standing();
            json!({
                "group": true,
                "role": "learner",
                "member": l.member(),
                "seq": seq,
                "applied": applied,
                "synced": !self.stale() && l.stopped().is_none(),
            })
        });
        let raft = local.convo_raft.as_ref().cloned().or_else(crate::convo::raft::global);
        let convo_raft = raft.as_ref().map_or(json!({ "enabled": false }), |m| {
            let (groups, leading, following) = m.summary();
            json!({ "enabled": true, "groups": groups, "leading": leading, "following": following })
        });
        let quic_addrs = local.reach.as_ref().map(|r| r.quic_addrs()).unwrap_or_default();
        srv::json(
            StatusCode::OK,
            &json!({
                "role": "runner+server",
                "machine": self.machine,
                "label": local.label,
                "upstream": self.upstream.url,
                "quic_addrs": quic_addrs,
                "store": store,
                "convo_raft": convo_raft,
                "version": env!("CARGO_PKG_VERSION"),
            }),
        )
    }

    fn convo_raft_status(&self, local: &LocalNode) -> Response {
        let raft = local.convo_raft.as_ref().cloned().or_else(crate::convo::raft::global);
        let groups = raft.as_ref().map_or_else(Vec::new, |m| m.all_status());
        let quic_addrs = local.reach.as_ref().map(|r| r.quic_addrs()).unwrap_or_default();
        srv::json(
            StatusCode::OK,
            &json!({
                "node": self.machine,
                "label": local.label,
                "quic_addrs": quic_addrs,
                "groups": groups,
            }),
        )
    }

    /// Initial-recipient routing check (Part 4): given a message command targeting `session` with
    /// optional UI-supplied `target_hint`, check if THIS node owns the session:
    /// - Match (local session or target == this node): enqueue directly in this node's local inbox.
    /// - Mismatch (target == another runner): DO NOT enqueue locally — forward directly over
    ///   `hyper-quic` (`reach.send_harness_message`, with seamless relayed reach fallback) so the
    ///   message is enqueued ONLY on the owning runner!
    async fn route_p2p_command(&self, session: &str, c: &crate::harness::model::Command, target_hint: Option<&str>, sender: &str) -> Option<P2pRoute> {
        use crate::reach::wire::P2pSendReply;
        if !crate::harness::session::is_message(c) {
            return None;
        }
        crate::harness::registry::message_id(c)?;
        let local = self.local.as_ref()?;
        // 1. Check whether this node holds the session locally.
        if local.host.as_ref().is_some_and(|h| h.get(session).is_some()) {
            let route = crate::harness::route::Route::new(&local.machine, sender);
            return match local.enqueue_local(session, None, c.clone(), route).await {
                P2pSendReply::Accepted { answer } => Some(P2pRoute::LocalAccepted(answer)),
                P2pSendReply::Refused { refusal } => Some(P2pRoute::LocalRefused(refusal)),
                P2pSendReply::NoSession | P2pSendReply::Error { .. } => None,
            };
        }
        // 2. Determine target runner from explicit UI hint or per-session Raft leader map.
        let raft_leader = local.convo_raft.as_ref().cloned().or_else(crate::convo::raft::global).and_then(|m| m.leader_of(session));
        let target_raw = target_hint.and_then(normalize_target).or_else(|| raft_leader.as_deref().and_then(normalize_target))?;
        // 3. Target matches this node, yet session isn't in local Host: never enqueue into wrong queue.
        if local.matches_target(target_raw) || target_raw == "server" {
            return None;
        }
        // 4. Target is ANOTHER runner: forward directly over `hyper-quic` WITHOUT local enqueue!
        let reach = local.reach.as_ref()?;
        let route = crate::harness::route::Route::new(target_raw, sender);
        let json = serde_json::to_value(c).ok().and_then(|v| route.attach(v, None))?;
        match reach.send_harness_message(target_raw, session, None, json).await {
            Ok(P2pSendReply::Accepted { answer }) => Some(P2pRoute::QuicAccepted { peer: target_raw.to_owned(), answer }),
            Ok(P2pSendReply::Refused { refusal }) => Some(P2pRoute::QuicRefused { peer: target_raw.to_owned(), refusal }),
            Ok(P2pSendReply::NoSession) => {
                tracing::debug!(session, peer = %target_raw, "P2P hyper-quic forward got NoSession; falling back to upstream");
                None
            }
            Ok(P2pSendReply::Error { why }) => {
                tracing::debug!(session, peer = %target_raw, %why, "P2P hyper-quic forward returned error; falling back to upstream");
                None
            }
            Err(e) => {
                tracing::debug!(session, peer = %target_raw, error = %format!("{e:#}"), "P2P hyper-quic forward failed; falling back to upstream");
                None
            }
        }
    }

    /// Intercept `POST /api/harness/sessions/:id/command` on a runner node: authenticate owner,
    /// check initial-recipient target match, handle locally or forward over direct `hyper-quic`
    /// without enqueueing locally on mismatch, and fall back seamlessly to upstream HTTP with the
    /// buffered body untouched if needed.
    async fn maybe_route_http_command(&self, req: Request) -> Result<Response, Box<Request>> {
        if self.local.is_none() || req.method() != Method::POST {
            return Err(Box::new(req));
        }
        let seg: Vec<String> = req.uri().path().trim_matches('/').split('/').filter(|s| !s.is_empty()).map(srv::decode).collect();
        let s: Vec<&str> = seg.iter().map(String::as_str).collect();
        let ["api", "harness", "sessions", session, "command"] = s.as_slice() else {
            return Err(Box::new(req));
        };
        let session = (*session).to_owned();
        if !self.owner(req.headers()).await {
            return Err(Box::new(req));
        }
        let header_target = req.headers().get(TARGET_HEADER).and_then(|v| v.to_str().ok()).map(str::to_owned);
        let (parts, body) = req.into_parts();
        let bytes = match srv::body_bytes(body, COMMAND_BODY_MAX).await {
            Ok(b) => b,
            Err(e) => return Ok(srv::text(StatusCode::PAYLOAD_TOO_LARGE, &format!("{e:#}"))),
        };
        if let Ok(mut val) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            let body_target = val.as_object_mut().and_then(|o| o.remove("target")).and_then(|v| v.as_str().map(str::to_owned));
            let target_hint = header_target.as_deref().or(body_target.as_deref());
            if let Ok(cmd) = serde_json::from_value::<crate::harness::model::Command>(val)
                && let Some(routed) = self.route_p2p_command(&session, &cmd, target_hint, "owner").await
            {
                let message = crate::harness::registry::message_id(&cmd);
                let resp = match routed {
                    P2pRoute::LocalAccepted(a) => {
                        let r = srv::json(StatusCode::OK, &crate::harness::registry::accepted(&message, &a));
                        srv::with_header(r, HeaderName::from_static(ROUTED_HEADER), &format!("local {}", self.machine))
                    }
                    P2pRoute::QuicAccepted { peer, answer } => {
                        let r = srv::json(StatusCode::OK, &crate::harness::registry::accepted(&message, &answer));
                        srv::with_header(r, HeaderName::from_static(ROUTED_HEADER), &format!("quic {peer}"))
                    }
                    P2pRoute::LocalRefused(refusal) => {
                        let (code, why) = crate::harness::ops::refused(&refusal);
                        let r = srv::json(code, &json!({ "error": why }));
                        srv::with_header(r, HeaderName::from_static(ROUTED_HEADER), &format!("local {}", self.machine))
                    }
                    P2pRoute::QuicRefused { peer, refusal } => {
                        let (code, why) = crate::harness::ops::refused(&refusal);
                        let r = srv::json(code, &json!({ "error": why }));
                        srv::with_header(r, HeaderName::from_static(ROUTED_HEADER), &format!("quic {peer}"))
                    }
                };
                return Ok(resp);
            }
        }
        // Fallback: forward the buffered request to upstream HTTP unchanged (zero local enqueue).
        let resp = self.forward_buffered(parts, bytes).await.unwrap_or_else(|e| {
            tracing::debug!(error = %format!("{e:#}"), "front door: kiln's server didn't answer");
            srv::text(StatusCode::BAD_GATEWAY, &format!("kiln's server ({}) didn't answer: {e:#}", self.upstream.url))
        });
        Ok(resp)
    }

    async fn forward_buffered(&self, parts: http::request::Parts, body: bytes::Bytes) -> anyhow::Result<Response> {
        let mut up = http::Request::builder().method(parts.method).uri(target(&parts.uri)).body(srv::full(body))?;
        *up.headers_mut() = without_hops(&parts.headers);
        let resp = self.send(up, false).await?;
        let (mut head, body) = resp.into_parts();
        for k in HOP {
            head.headers.remove(k);
        }
        Ok(http::Response::from_parts(head, body.map_err(std::io::Error::other).boxed()))
    }

    /// Whether the request's credentials are the owner's, as the server says (kept a while).
    async fn owner(&self, headers: &HeaderMap) -> bool {
        let get = |h: header::HeaderName| headers.get(h).and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
        let (cookie, auth) = (get(header::COOKIE), get(header::AUTHORIZATION));
        if cookie.is_empty() && auth.is_empty() {
            return false;
        }
        let key = crate::util::sha256_hex(format!("{cookie}\n{auth}").as_bytes());
        if let Some((owner, at)) = self.owners.load().get(&key).copied()
            && at.elapsed() < OWNER_FOR
        {
            return owner;
        }
        let asked = async {
            let mut req = http::Request::builder().method(Method::GET).uri("/api/cluster/owner").body(srv::empty())?;
            for (k, v) in [(header::HOST, self.authority()), (header::COOKIE, cookie), (header::AUTHORIZATION, auth)] {
                if !v.is_empty() {
                    req.headers_mut().insert(k, HeaderValue::from_str(&v)?);
                }
            }
            let resp = self.send(req, true).await?;
            anyhow::ensure!(resp.status().is_success(), "the server answered {}", resp.status());
            let body = resp.into_body().collect().await?.to_bytes();
            Ok::<_, anyhow::Error>(serde_json::from_slice::<serde_json::Value>(&body)?.get("owner").and_then(serde_json::Value::as_bool).unwrap_or(false))
        };
        // Not known (the server away): not the owner's, for now, and asked again next time.
        let Ok(owner) = asked.await else { return false };
        let now = Instant::now();
        self.owners.update(|owners| {
            if owners.len() >= OWNERS {
                owners.retain(|_, (_, at)| at.elapsed() < OWNER_FOR);
                if owners.len() >= OWNERS {
                    owners.clear();
                }
            }
            owners.insert(key.clone(), (owner, now));
        });
        owner
    }

    fn authority(&self) -> String {
        match (self.upstream.tls.is_some(), self.upstream.port) {
            (true, 443) | (false, 80) => self.upstream.host.clone(),
            (_, port) => format!("{}:{port}", self.upstream.host),
        }
    }

    /// `up` to the server, its connection kept once the answer is read. A read without a body
    /// (`again`) goes on a kept connection when one is free, and once more on a new one if that
    /// one was gone (the server closes one idle 20 s); anything else goes on a new connection, as
    /// it can't go twice.
    async fn send(&self, up: http::Request<srv::Body>, again: bool) -> anyhow::Result<http::Response<Incoming>> {
        let (s, resp) = match again.then(|| self.kept()).flatten() {
            Some(mut s) => {
                let (method, uri, headers) = (up.method().clone(), up.uri().clone(), up.headers().clone());
                match s.send_request(up).await {
                    Ok(resp) => (s, resp),
                    Err(_) => {
                        let mut up = http::Request::builder().method(method).uri(uri).body(srv::empty())?;
                        *up.headers_mut() = headers;
                        let mut s = self.dial().await?;
                        let resp = s.send_request(up).await?;
                        (s, resp)
                    }
                }
            }
            None => {
                let mut s = self.dial().await?;
                let resp = s.send_request(up).await?;
                (s, resp)
            }
        };
        // Kept for the next request; one past IDLE kept is let go.
        let _ = self.idle.try_send(s);
        Ok(resp)
    }

    /// A kept connection that takes a request now; the ones the server closed let go.
    fn kept(&self) -> Option<SendRequest<srv::Body>> {
        // Each kept one looked at once at most: a closed one goes, one not ready yet goes back.
        for _ in 0..self.idle_out.len() {
            let Ok(s) = self.idle_out.try_recv() else { return None };
            if s.is_closed() {
                continue;
            }
            if s.is_ready() {
                return Some(s);
            }
            let _ = self.idle.try_send(s);
        }
        None
    }

    /// A new connection to the server (HTTP/1.1; TLS for an `https://` server).
    async fn dial(&self) -> anyhow::Result<SendRequest<srv::Body>> {
        let (host, port, https) = (self.upstream.host.as_str(), self.upstream.port, self.upstream.tls.is_some());
        let tcp = redial(|| crate::http::io::connect_out(host, port, https)).await?;
        let sender = if https {
            let stream = TlsStream::connect(RtFuturesIo::new(tcp), self.tls().config()?, crate::http::tls::server_name(host)?).await.map_err(|e| anyhow::anyhow!("TLS to {host}: {e}"))?;
            let (sender, conn) = kiln_http::client::http1::handshake::<_, srv::Body>(stream).await?;
            crate::rt::spawn_local(async move {
                let _ = conn.await;
            })
            .map_err(|e| anyhow::anyhow!("spawn connection task: {e}"))?;
            sender
        } else {
            let (sender, conn) = kiln_http::client::http1::handshake::<_, srv::Body>(RtIo::new(tcp)).await?;
            crate::rt::spawn_local(async move {
                let _ = conn.await;
            })
            .map_err(|e| anyhow::anyhow!("spawn connection task: {e}"))?;
            sender
        };
        Ok(sender)
    }

    /// `req` to the server as it came: a WebSocket piped (intercepting session message submits for
    /// direct local / `hyper-quic` forwarding), anything else streamed.
    async fn forward(self: Arc<Self>, req: Request) -> Response {
        let r = if crate::http::ws::is_upgrade(&req) {
            Arc::clone(&self).forward_ws(req).await
        } else {
            self.forward_http(req).await
        };
        r.unwrap_or_else(|e| {
            tracing::debug!(error = %format!("{e:#}"), "front door: kiln's server didn't answer");
            srv::text(StatusCode::BAD_GATEWAY, &format!("kiln's server ({}) didn't answer: {e:#}", self.upstream.url))
        })
    }

    async fn forward_http(&self, req: Request) -> anyhow::Result<Response> {
        let (parts, body) = req.into_parts();
        // A read with nothing to send can go again; a write might have been done the first time.
        let again = matches!(parts.method, Method::GET | Method::HEAD) && body.is_end_stream();
        let mut up = http::Request::builder().method(parts.method.clone()).uri(target(&parts.uri)).body(body.map_err(std::io::Error::other).boxed())?;
        *up.headers_mut() = without_hops(&parts.headers);
        let resp = match self.send(up, again).await {
            Ok(resp) => resp,
            Err(e) => {
                // Offline resilience: if upstream is unreachable on a GET route the local Raft
                // replica can answer, serve from our local SQLite replica marked `kiln-read-stale: 1`.
                if again
                    && let Some(stale) = self.read_stale(&parts).await
                {
                    return Ok(stale);
                }
                return Err(e);
            }
        };
        let (mut head, body) = resp.into_parts();
        for k in HOP {
            head.headers.remove(k);
        }
        Ok(http::Response::from_parts(head, body.map_err(std::io::Error::other).boxed()))
    }

    /// Fallback read from local Raft replica when upstream is unreachable (offline resilience).
    async fn read_stale(&self, parts: &http::request::Parts) -> Option<Response> {
        let learner = self.active_learner()?;
        let seg: Vec<String> = parts.uri.path().trim_matches('/').split('/').filter(|s| !s.is_empty()).map(srv::decode).collect();
        let s: Vec<&str> = seg.iter().map(String::as_str).collect();
        if !crate::reads::answers(&s) {
            return None;
        }
        // Check cached owner verdict (no network call since upstream is down).
        let get = |h: header::HeaderName| parts.headers.get(h).and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
        let (cookie, auth) = (get(header::COOKIE), get(header::AUTHORIZATION));
        if cookie.is_empty() && auth.is_empty() {
            return None;
        }
        let key = crate::util::sha256_hex(format!("{cookie}\n{auth}").as_bytes());
        if !self.owners.load().get(&key).is_some_and(|(ok, _)| *ok) {
            return None;
        }
        let query: HashMap<String, String> = parts
            .uri
            .query()
            .unwrap_or("")
            .split('&')
            .filter(|kv| !kv.is_empty())
            .map(|kv| {
                let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                (srv::decode(k), srv::decode(v))
            })
            .collect();
        let v = crate::reads::answer(learner.store(), &s, &query).await?;
        let r = srv::json(StatusCode::OK, &v);
        let r = srv::with_header(r, HeaderName::from_static(READ_HEADER), &format!("{} {}", self.machine, learner.standing().0));
        Some(srv::with_header(r, HeaderName::from_static(STALE_HEADER), "1"))
    }

    /// A WebSocket: the upgrade asked of the server first, its answer the browser's; then both
    /// connections piped until either ends. For `/api/harness/sessions/:id/ws` on a runner node,
    /// incoming browser `SessionUp::Under` message frames are intercepted so submits to this node
    /// enqueue locally and submits to another runner forward directly over `hyper-quic` without
    /// bouncing through the upstream WebSocket or enqueueing on the receiving node.
    async fn forward_ws(self: Arc<Self>, mut req: Request) -> anyhow::Result<Response> {
        let seg: Vec<String> = req.uri().path().trim_matches('/').split('/').filter(|s| !s.is_empty()).map(srv::decode).collect();
        let s: Vec<&str> = seg.iter().map(String::as_str).collect();
        let intercept_session = if self.local.is_some()
            && let ["api", "harness", "sessions", session, "ws"] = s.as_slice()
            && self.owner(req.headers()).await
        {
            Some((*session).to_owned())
        } else {
            None
        };
        let browser = kiln_http::upgrade::on(&mut req);
        let (parts, _) = req.into_parts();
        let mut up = http::Request::builder().method(Method::GET).uri(target(&parts.uri)).body(srv::empty())?;
        let mut headers = without_hops(&parts.headers);
        if intercept_session.is_some() {
            // Uncompressed RFC 6455 frames when inspecting text commands; snapshots remain `?z=gzip`.
            headers.remove(header::SEC_WEBSOCKET_EXTENSIONS);
        }
        headers.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
        headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        *up.headers_mut() = headers;
        let (host, port) = (self.upstream.host.clone(), self.upstream.port);
        let tcp = redial(|| crate::http::io::connect(&host, port)).await?;
        let mut sender = match &self.upstream.tls {
            Some(ct) => {
                let stream = TlsStream::connect(RtFuturesIo::new(tcp), ct.config()?, crate::http::tls::server_name(&host)?).await.map_err(|e| anyhow::anyhow!("TLS to {host}: {e}"))?;
                let (sender, conn) = kiln_http::client::http1::handshake::<_, srv::Body>(stream).await?;
                crate::rt::spawn_local(async move {
                    let _ = conn.with_upgrades().await;
                })
                .map_err(|e| anyhow::anyhow!("spawn connection task: {e}"))?;
                sender
            }
            None => {
                let (sender, conn) = kiln_http::client::http1::handshake::<_, srv::Body>(RtIo::new(tcp)).await?;
                crate::rt::spawn_local(async move {
                    let _ = conn.with_upgrades().await;
                })
                .map_err(|e| anyhow::anyhow!("spawn connection task: {e}"))?;
                sender
            }
        };
        let mut resp = sender.send_request(up).await?;
        if resp.status() != StatusCode::SWITCHING_PROTOCOLS {
            let (mut head, body) = resp.into_parts();
            for k in HOP {
                head.headers.remove(k);
            }
            return Ok(http::Response::from_parts(head, body.map_err(std::io::Error::other).boxed()));
        }
        let server = kiln_http::upgrade::on(&mut resp);
        let mut out = http::Response::new(srv::empty());
        *out.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
        for k in [header::UPGRADE, header::CONNECTION, header::SEC_WEBSOCKET_ACCEPT, header::SEC_WEBSOCKET_PROTOCOL, header::SEC_WEBSOCKET_EXTENSIONS] {
            if intercept_session.is_some() && k == header::SEC_WEBSOCKET_EXTENSIONS {
                continue;
            }
            if let Some(v) = resp.headers().get(&k) {
                out.headers_mut().insert(k, v.clone());
            }
        }
        let door = Arc::clone(&self);
        crate::rt::spawn_local(async move {
            if let (Ok(b), Ok(s)) = (browser.await, server.await) {
                let res = match intercept_session {
                    Some(session) => pipe_session_ws(door, session, b, s).await,
                    None => super::proxy::pipe(b, s).await.map(|_| ()).map_err(anyhow::Error::from),
                };
                if let Err(e) = res {
                    tracing::debug!(error = %e, "front door: a WebSocket to kiln's server ended");
                }
            }
        })
        .map_err(|e| anyhow::anyhow!("spawn WebSocket pipe: {e}"))?;
        Ok(out)
    }
}

/// Frame-aware WebSocket bridge for `/api/harness/sessions/:id/ws`: streams all server->browser
/// events unchanged, while intercepting browser `SessionUp::Under` message submits to route
/// directly locally or over `hyper-quic` to the session's owning runner (never enqueueing locally
/// on a non-owning node).
async fn pipe_session_ws(door: Arc<Door>, session: String, browser_io: kiln_http::upgrade::Upgraded, upstream_io: kiln_http::upgrade::Upgraded) -> anyhow::Result<()> {
    use tungstenite::protocol::{Role, WebSocketConfig};
    let cfg = WebSocketConfig::default().max_message_size(Some(COMMAND_BODY_MAX)).max_frame_size(Some(COMMAND_BODY_MAX));
    let browser_ws = async_tungstenite::WebSocketStream::from_raw_socket(browser_io, Role::Server, Some(cfg)).await;
    let upstream_ws = async_tungstenite::WebSocketStream::from_raw_socket(upstream_io, Role::Client, Some(cfg)).await;
    let (mut b_tx, mut b_rx) = browser_ws.split();
    let (mut u_tx, mut u_rx) = upstream_ws.split();
    let (ack_tx, ack_rx) = crate::memory::queue::unbounded::<String>(&crate::memory::owners::CHANNELS);
    let kind = crate::harness::asked::Kind { message: true, deduped: true };

    loop {
        futures::select_biased! {
            ack = ack_rx.recv().fuse() => {
                if let Ok(frame) = ack
                    && b_tx.send(crate::http::ws::text(frame)).await.is_err()
                {
                    return Ok(());
                }
            }
            from_up = u_rx.next().fuse() => match from_up {
                Some(Ok(msg)) => {
                    if b_tx.send(msg).await.is_err() {
                        return Ok(());
                    }
                }
                Some(Err(_)) | None => {
                    let _ = b_tx.close(None).await;
                    return Ok(());
                }
            },
            from_browser = b_rx.next().fuse() => match from_browser {
                Some(Ok(crate::http::ws::Message::Text(text))) => {
                    if let Ok(crate::wire::ui::SessionUp::Under { id: cmd, c, target, .. }) = crate::wire::ui::session_up(text.as_str())
                        && crate::harness::session::is_message(&c)
                        && let Some(routed) = door.route_p2p_command(&session, &c, target.as_deref(), "owner").await
                    {
                        let sent = match routed {
                            P2pRoute::LocalAccepted(a) | P2pRoute::QuicAccepted { answer: a, .. } => crate::harness::registry::Sent::Accepted(a),
                            P2pRoute::LocalRefused(r) | P2pRoute::QuicRefused { refusal: r, .. } => crate::harness::registry::Sent::Refused(r),
                        };
                        let _ = ack_tx.try_send(crate::harness::asked::frame(&cmd, &sent, kind));
                        continue;
                    }
                    if u_tx.send(crate::http::ws::Message::Text(text)).await.is_err() {
                        return Ok(());
                    }
                }
                Some(Ok(msg)) => {
                    if u_tx.send(msg).await.is_err() {
                        return Ok(());
                    }
                }
                Some(Err(_)) | None => {
                    let _ = u_tx.close(None).await;
                    return Ok(());
                }
            },
        }
    }
}

impl srv::Handler for Door {
    async fn handle(self: Arc<Self>, req: Request) -> Response {
        if let Some(r) = self.read(&req).await {
            return r;
        }
        let req = match self.maybe_route_http_command(req).await {
            Ok(resp) => return resp,
            Err(req) => *req,
        };
        self.forward(req).await
    }
}

/// Start a runner node's own local HTTP server (`0.0.0.0:7070` by default) serving kiln's unified
/// UI/API entrypoint backed by its cluster Raft learner, per-session conversation Raft store, and
/// direct `hyper-quic` peer-to-peer message forwarding.
///
/// If `addr` is already bound (e.g., a co-located `kiln server` holds `:7070`), returns `Ok(None)`
/// cleanly without failing the runner.
#[allow(clippy::disallowed_methods)] // (b) accept backoff on transient OS accept failure
pub fn listen_node_http(addr: &str, upstream: Upstream, local: LocalNode, shutdown: Shutdown) -> anyhow::Result<Option<Listening>> {
    let parsed: SocketAddr = addr.parse().map_err(|e| anyhow::anyhow!("invalid node HTTP address {addr:?}: {e}"))?;
    let std_listener = match std::net::TcpListener::bind(parsed) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            tracing::info!(%addr, "node HTTP address is already bound (co-located server); skipping duplicate listener");
            return Ok(None);
        }
        Err(e) => return Err(anyhow::anyhow!("bind node HTTP listener {addr}: {e}")),
    };
    let bound = std_listener.local_addr()?;
    let door = Arc::new(Door::for_node(upstream, local)?);
    let stop = Shutdown::new();
    let (sd, parent) = (stop.clone(), shutdown);
    let open = Arc::new(AtomicUsize::new(0));
    crate::rt::spawn(async move {
        let listener = match hyper_rt::tcp::TcpListener::from_owned(hyper_rt::tcp::OwnedStream::from(std_listener), 256) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(%bound, error = %e, "node HTTP: cannot hand listener to runtime");
                return;
            }
        };
        tracing::info!(%bound, "node HTTP server listening (unified fleet entrypoint + direct hyper-quic forwarding)");
        loop {
            let accepted = futures::select_biased! {
                () = sd.cancelled().fuse() => return,
                () = parent.cancelled().fuse() => return,
                r = listener.accept().fuse() => r,
            };
            let stream = match accepted {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(error = %e, "node HTTP: accept failed");
                    crate::rt::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            if open.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
                continue;
            }
            open.fetch_add(1, Ordering::Relaxed);
            let (door, sd, n) = (Arc::clone(&door), sd.clone(), Arc::clone(&open));
            crate::rt::spawn_spread(async move {
                let remote = stream.peer_addr().ok();
                let _ = crate::discovery::until_stopped(&sd, srv::serve_connection(RtIo::new(stream), door, remote)).await;
                n.fetch_sub(1, Ordering::Relaxed);
            });
        }
    });
    Ok(Some(Listening { stop, bound }))
}

fn target(uri: &http::Uri) -> String {
    uri.path_and_query().map_or("/", http::uri::PathAndQuery::as_str).to_owned()
}

/// `connect` tried up to [`DIALS`] times, a moment apart: a server on a busy machine takes a new
/// connection late or drops its first packets, and the next try often gets through.
#[allow(clippy::disallowed_methods)] // (b) redial, linear backoff, picked
async fn redial<T, F: std::future::Future<Output = anyhow::Result<T>>>(mut connect: impl FnMut() -> F) -> anyhow::Result<T> {
    let mut tried = 1;
    loop {
        match connect().await {
            Ok(t) => return Ok(t),
            Err(e) if tried >= DIALS => return Err(e),
            Err(e) => {
                tracing::debug!(error = %format!("{e:#}"), tried, "front door: kiln's server didn't take a connection; trying again");
                crate::rt::sleep(Duration::from_millis(200 * u64::from(tried))).await;
                tried += 1;
            }
        }
    }
}

/// A request's headers as they go on: all but those about its connection.
fn without_hops(incoming: &HeaderMap) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (k, v) in incoming {
        if !HOP.contains(&k.as_str()) {
            h.append(k, v.clone());
        }
    }
    h
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use anyhow::Context as _;

    use super::*;
    use crate::pipeline;
    use crate::proto::AgentMsg;
    use crate::store::group::{self, Fetch, Group};
    use crate::store::learner::{self as wire, Learners, Msg};
    use crate::store::{Repo, Run, Status, Store};

    /// kiln's server, standing in: whoever brings the cookie `kiln_session=owner` is the owner, and
    /// anything else it answers says it did.
    fn upstream() -> anyhow::Result<String> {
        let l = std::net::TcpListener::bind("127.0.0.1:0")?;
        let url = format!("http://{}", l.local_addr()?);
        std::thread::spawn(move || {
            for mut s in l.incoming().flatten() {
                let mut buf = [0u8; 8192];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(buf.get(..n).unwrap_or_default()).into_owned();
                let path = req.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let owner = req.to_ascii_lowercase().contains("cookie: kiln_session=owner");
                let body = if path == "/api/cluster/owner" { format!(r#"{{"owner":{owner}}}"#) } else { format!("upstream {path}") };
                let _ = write!(s, "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
            }
        });
        Ok(url)
    }

    /// The door, serving plain HTTP on a port of its own (at the front door it serves HTTPS).
    fn serve(door: Arc<Door>) -> anyhow::Result<SocketAddr> {
        let l = std::net::TcpListener::bind("127.0.0.1:0")?;
        let at = l.local_addr()?;
        crate::rt::block_on(async move {
            crate::rt::spawn(async move {
                let Ok(listener) = hyper_rt::tcp::TcpListener::from_owned(hyper_rt::tcp::OwnedStream::from(l), 64) else { return };
                while let Ok(stream) = listener.accept().await {
                    let door = Arc::clone(&door);
                    crate::rt::spawn(async move {
                        let remote = stream.peer_addr().ok();
                        srv::serve_connection(crate::http::io::RtIo::new(stream), door, remote).await;
                    });
                }
            });
            Ok(())
        })?;
        Ok(at)
    }

    /// `GET path` of the door: the status, the head (lowercased) and the body.
    fn get(at: SocketAddr, path: &str, cookie: Option<&str>) -> anyhow::Result<(u16, String, String)> {
        let mut s = std::net::TcpStream::connect(at)?;
        s.set_read_timeout(Some(Duration::from_secs(20)))?;
        let cookie = cookie.map(|c| format!("cookie: {c}\r\n")).unwrap_or_default();
        write!(s, "GET {path} HTTP/1.1\r\nhost: kiln.test\r\n{cookie}connection: close\r\n\r\n")?;
        let mut out = String::new();
        s.read_to_string(&mut out)?;
        let (head, body) = out.split_once("\r\n\r\n").unwrap_or((out.as_str(), ""));
        let status = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        Ok((status, head.to_ascii_lowercase(), body.to_owned()))
    }

    /// `POST path` of the door with `body`: the status and the body.
    fn post(at: SocketAddr, path: &str, body: &str) -> anyhow::Result<(u16, String)> {
        let mut s = std::net::TcpStream::connect(at)?;
        s.set_read_timeout(Some(Duration::from_secs(20)))?;
        write!(s, "POST {path} HTTP/1.1\r\nhost: kiln.test\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len())?;
        let mut out = String::new();
        s.read_to_string(&mut out)?;
        let (head, body) = out.split_once("\r\n\r\n").unwrap_or((out.as_str(), ""));
        Ok((head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0), body.to_owned()))
    }

    /// kiln's server, standing in, keeping each connection for the next request: an answer says
    /// its path and the connection it came on (counted from 1). `close_second`: a connection's
    /// second request is read and the connection closed unanswered, as a server closing a kept
    /// connection as a request goes on it.
    fn kept_upstream(close_second: bool) -> anyhow::Result<(String, Arc<AtomicUsize>)> {
        let l = std::net::TcpListener::bind("127.0.0.1:0")?;
        let url = format!("http://{}", l.local_addr()?);
        let conns = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&conns);
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                let n = counted.fetch_add(1, Ordering::SeqCst) + 1;
                std::thread::spawn(move || answer(s, n, close_second));
            }
        });
        Ok((url, conns))
    }

    /// One connection's requests, answered in turn ([`kept_upstream`]).
    fn answer(mut s: std::net::TcpStream, conn: usize, close_second: bool) {
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        for asked in 1.. {
            let head_end = loop {
                if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break i + 4;
                }
                match s.read(&mut chunk) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => buf.extend_from_slice(chunk.get(..n).unwrap_or_default()),
                }
            };
            let head = String::from_utf8_lossy(buf.get(..head_end).unwrap_or_default()).to_ascii_lowercase();
            let len = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(0);
            while buf.len() < head_end + len {
                match s.read(&mut chunk) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => buf.extend_from_slice(chunk.get(..n).unwrap_or_default()),
                }
            }
            let path = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
            buf = buf.get(head_end + len..).map(<[u8]>::to_vec).unwrap_or_default();
            if close_second && asked == 2 {
                return;
            }
            let body = format!("upstream {path} on {conn}");
            if write!(s, "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\n\r\n{body}", body.len()).is_err() {
                return;
            }
        }
    }

    /// The door, its replica a bare copy of a server's records: what's asked of it here is
    /// forwarded, as every request but the owner's reads is.
    fn door_to(url: &str, dir: &std::path::Path) -> anyhow::Result<Arc<Door>> {
        let server = Store::open_grouped(&dir.join("kiln.db"), &dir.join("cluster"))?;
        let fetch: Fetch = Arc::new(move |dest: &std::path::Path| {
            let (s, d) = (server.clone(), dest.to_path_buf());
            crate::rt::block_on(async move { s.copy_for_learner(d).await })
        });
        let db = dir.join("runner").join("kiln.db");
        let replica = Arc::new(Group::learner(&db, &dir.join("runner").join("cluster"), group::member_of("runner-1"), fetch)?);
        let (learner, _) = Learner::around(replica, db, "runner-1")?;
        Ok(Arc::new(Door::new(Upstream::parse(url, &[])?, Arc::new(learner))?))
    }

    /// P2: a page's pictures and frames go to the server on a connection earlier requests opened,
    /// not each on a new one: new connections to a busy server came late, and phones, whose HTTPS
    /// is the front door, lost most of a conversation's pictures and pages to 502s.
    #[test]
    fn p2_forwarded_requests_go_on_kept_connections() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (url, conns) = kept_upstream(false)?;
        let at = serve(door_to(&url, dir.path())?)?;
        for i in 0..6 {
            let path = format!("/api/harness/sessions/s/files/{i}");
            let (status, head, body) = get(at, &path, Some("kiln_session=owner"))?;
            assert_eq!((status, body), (200, format!("upstream {path} on 1")), "{head}");
        }
        assert_eq!(conns.load(Ordering::SeqCst), 1, "the six went on one connection");
        Ok(())
    }

    /// P2: a connection the server didn't take (a busy machine drops a new one's first packets, a
    /// server starting refuses it) is tried again before the request is answered 502.
    #[test]
    fn p2_a_connection_the_server_did_not_take_is_tried_again() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        // An address nothing listens on yet: the server comes up there after the first try.
        let addr = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?;
        let at = serve(door_to(&format!("http://{addr}"), dir.path())?)?;
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            if let Ok((s, _)) = std::net::TcpListener::bind(addr).and_then(|l| l.accept()) {
                answer(s, 1, false);
            }
        });
        let (status, head, body) = get(at, "/v/0123/page.html", None)?;
        assert_eq!((status, body.as_str()), (200, "upstream /v/0123/page.html on 1"), "{head}");
        Ok(())
    }

    /// P2: a read that went on a kept connection the server closed as it went (it closes one idle
    /// 20 s) goes again on a new connection; a write goes on a new connection from the first, as
    /// it can't go twice.
    #[test]
    fn p2_a_read_a_kept_connection_lost_goes_again_and_a_write_goes_on_a_new_one() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        // Each connection's second request is read and the connection closed unanswered.
        let (url, conns) = kept_upstream(true)?;
        let at = serve(door_to(&url, dir.path())?)?;
        // Which connection each went on. Not always the first, second and third: on a loaded
        // machine a connection the server didn't take in time is tried again first (as
        // p2_a_connection_the_server_did_not_take_is_tried_again holds).
        let on = |status: u16, body: &str, path: &str| -> anyhow::Result<usize> {
            body.strip_prefix(&format!("upstream {path} on ")).and_then(|n| n.parse().ok()).filter(|_| status == 200).ok_or_else(|| anyhow::anyhow!("{path}: {status} {body}"))
        };
        let (status, _, body) = get(at, "/api/a", None)?;
        let a = on(status, &body, "/api/a")?;
        let (status, _, body) = get(at, "/api/b", None)?;
        let b = on(status, &body, "/api/b")?;
        assert!(b > a, "sent again, on a new connection: /api/a on {a}, /api/b on {b}");
        // Not on the second connection, kept, which would drop it.
        let (status, body) = post(at, "/api/c", "{}")?;
        let c = on(status, &body, "/api/c")?;
        assert!(c > b, "a write on a new connection: /api/b on {b}, /api/c on {c}");
        assert_eq!(conns.load(Ordering::SeqCst), c, "none after the write's");
        Ok(())
    }

    fn add_run(store: &Store, id: &str) -> anyhow::Result<()> {
        let p = pipeline::parse("name: ci\njobs:\n  a: {steps: [{run: x}]}\n")?;
        let run = Run {
            id: id.into(),
            repo_id: "r1".into(),
            repo_name: "hyper-light/x".into(),
            number: 0,
            pipeline: "ci".into(),
            event: "push".into(),
            branch: "main".into(),
            sha: "abc".into(),
            message: String::new(),
            author: String::new(),
            status: Status::Queued,
            created_at: 0,
            started_at: None,
            finished_at: None,
            error: None,
            concurrency_group: None,
        };
        let s = store.clone();
        crate::rt::block_on(async move {
            s.create_run(run, pipeline::plan(&p, &pipeline::Context::default())?).await?;
            Ok(())
        })
    }

    #[test]
    fn the_owner_s_reads_are_answered_from_the_replica_and_everything_else_by_the_server() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let server = Store::open_grouped(&dir.path().join("kiln.db"), &dir.path().join("cluster"))?;
        let s = server.clone();
        crate::rt::block_on(async move {
            s.upsert_repo(Repo {
                id: "r1".into(),
                name: "hyper-light/x".into(),
                clone_url: "https://example/x".into(),
                branches: vec![],
                poll_seconds: 60,
                enabled: true,
                created_at: 0,
                last_polled_at: None,
                last_error: None,
                mode: "upstream".into(),
                default_branch: None,
                mirror: None,
            })
            .await?;
            Ok(())
        })?;
        add_run(&server, "first-run")?;
        // The runner's learner, from a copy of the server's kiln.db.
        let member = group::member_of("runner-1");
        let g = server.group().context("grouped")?;
        g.learners([member].into());
        let end = Instant::now() + Duration::from_secs(30);
        while !g.point().1.learners.contains(&member) {
            anyhow::ensure!(Instant::now() < end, "the learner never joined");
            std::thread::sleep(Duration::from_millis(10));
        }
        let fetch: Fetch = {
            let server = server.clone();
            Arc::new(move |dest: &std::path::Path| {
                let (s, d) = (server.clone(), dest.to_path_buf());
                crate::rt::block_on(async move { s.copy_for_learner(d).await })
            })
        };
        let db = dir.path().join("runner").join("kiln.db");
        let member_group = Arc::new(Group::learner(&db, &dir.path().join("runner").join("cluster"), member, fetch)?);
        let (learner, said) = Learner::around(member_group, db, "runner-1")?;
        let learner = Arc::new(learner);
        // The runner's link to the server: the group's messages both ways, and where kiln.db stands.
        let done = Arc::new(AtomicBool::new(false));
        let link = {
            let (server, learner, done, learners) = (server.clone(), Arc::clone(&learner), Arc::clone(&done), Learners::default());
            std::thread::spawn(move || {
                let Some(g) = server.group() else { return };
                let to_runner = g.outgoing();
                let mut id = 0;
                while !done.load(Ordering::SeqCst) {
                    let mut moved = false;
                    while let Ok((to, bytes)) = to_runner.try_recv() {
                        moved = true;
                        id += 1;
                        if to == member {
                            for (m, raft) in wire::frames(id, bytes) {
                                learner.heard(&m, raft);
                            }
                        }
                    }
                    while let Ok(AgentMsg::Cluster { json, raft }) = said.try_recv() {
                        moved = true;
                        match wire::decode(&json) {
                            Some(Msg::Ask { id, .. }) => learner.heard(&Msg::At { id, seq: g.standing().0 }, Vec::new()),
                            Some(m) => learners.heard(g, "runner-1", &m, raft),
                            None => {}
                        }
                    }
                    if !moved {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            })
        };
        let door = Arc::new(Door::new(Upstream::parse(&upstream()?, &[])?, Arc::clone(&learner))?);
        let at = serve(door)?;
        let owner = Some("kiln_session=owner");

        let (status, head, body) = get(at, "/api/runs", owner)?;
        assert_eq!(status, 200, "{head}\n{body}");
        assert!(head.contains("kiln-read: runner-1"), "answered by the runner's replica: {head}");
        assert!(body.contains("first-run"), "{body}");
        // A write the server just made is in the next read: the replica holds what the server held.
        add_run(&server, "second-run")?;
        let (_, head, body) = get(at, "/api/runs", owner)?;
        assert!(head.contains("kiln-read: runner-1") && body.contains("second-run"), "{head}\n{body}");
        let (_, head, body) = get(at, "/api/runs/second-run", owner)?;
        assert!(head.contains("kiln-read") && body.contains("\"jobs\""), "{head}\n{body}");
        // The CI pages' own list too.
        let (_, head, body) = get(at, "/api/ci/runs", owner)?;
        assert!(head.contains("kiln-read") && body.contains("\"runs\""), "{head}\n{body}");

        // Everything else is the server's, as is anyone's but the owner's.
        let (_, head, body) = get(at, "/api/agents", owner)?;
        assert!(!head.contains("kiln-read") && body.contains("upstream /api/agents"), "{head}\n{body}");
        let (_, head, body) = get(at, "/api/runs", Some("kiln_session=someone"))?;
        assert!(!head.contains("kiln-read") && body.contains("upstream /api/runs"), "{head}\n{body}");
        let (_, _, body) = get(at, "/api/runs", None)?;
        assert!(body.contains("upstream /api/runs"), "{body}");
        let (_, _, body) = get(at, "/api/runs/no-such-run", owner)?;
        assert!(body.contains("upstream /api/runs/no-such-run"), "a run the replica hasn't is the server's to say: {body}");

        done.store(true, Ordering::SeqCst);
        link.join().ok();
        Ok(())
    }

    /// Part 4: initial-recipient routing check on `Door` (`localhost:7070`). When the browser posts
    /// a message to node A (`laptop-a`) for a session owned by node B (`laptop-b`), node A checks
    /// target != self, does ZERO local enqueue on node A, and forwards directly to node B over
    /// `hyper-quic` (`kiln-routed: quic laptop-b`), returning node B's inbox receipt.
    #[test]
    fn p2p_door_routes_remote_session_directly_over_hyper_quic_without_local_enqueue() -> anyhow::Result<()> {
        let dir_a = tempfile::tempdir()?;
        let dir_b = tempfile::tempdir()?;
        let up_url = upstream()?;

        let (b_send_tx, b_send_rx) = crate::memory::queue::unbounded::<crate::reach::quic::HarnessSendAsk>(&crate::memory::owners::CHANNELS);
        let (a_send_tx, a_send_rx) = crate::memory::queue::unbounded::<crate::reach::quic::HarnessSendAsk>(&crate::memory::owners::CHANNELS);

        // Start two real reach nodes with direct `hyper-quic` endpoints enabled on loopback.
        let cfg_a = crate::reach::node::Config {
            harness_send: Some(a_send_tx),
            sense: false,
            direct_quic: true,
            ..crate::reach::node::Config::for_runner("laptop-a", dir_a.path(), true)
        };
        let cfg_b = crate::reach::node::Config {
            harness_send: Some(b_send_tx),
            sense: false,
            direct_quic: true,
            ..crate::reach::node::Config::for_runner("laptop-b", dir_b.path(), true)
        };
        let (handle_a, _out_a) = crate::reach::node::start(cfg_a)?;
        let (handle_b, _out_b) = crate::reach::node::start(cfg_b)?;

        let quic_a = handle_a.peer_quic().context("laptop-a hyper-quic")?.clone();
        let quic_b = handle_b.peer_quic().context("laptop-b hyper-quic")?.clone();
        let addr_a = SocketAddr::from(([127, 0, 0, 1], quic_a.local_addr().port()));
        let addr_b = SocketAddr::from(([127, 0, 0, 1], quic_b.local_addr().port()));
        quic_a.update_peers(vec![crate::reach::quic::PeerEntry {
            name: "laptop-b".into(),
            fingerprint: handle_b.fingerprint().to_owned(),
            addrs: vec![addr_b],
        }]);
        quic_b.update_peers(vec![crate::reach::quic::PeerEntry {
            name: "laptop-a".into(),
            fingerprint: handle_a.fingerprint().to_owned(),
            addrs: vec![addr_a],
        }]);

        // Track deliveries on both nodes: laptop-a must receive 0, laptop-b must receive 1!
        let b_received = Arc::new(AtomicUsize::new(0));
        let b_cnt = Arc::clone(&b_received);
        crate::rt::block_on(async move {
            crate::rt::spawn(async move {
                while let Ok(ask) = b_send_rx.recv().await {
                    b_cnt.fetch_add(1, Ordering::SeqCst);
                    let reply = crate::reach::wire::P2pSendReply::Accepted {
                        answer: crate::harness::inbox::Answer {
                            session: ask.session,
                            message: "m-quic-1".into(),
                            epoch: 1,
                            at: 1000,
                            seq: 1,
                            repeat: false,
                        },
                    };
                    let _ = ask.reply.send(reply);
                }
            });
            Ok(())
        })?;

        // Build laptop-a's per-session Raft manager and LocalNode (laptop-a does NOT own sess-b).
        let raft_a = crate::convo::raft::Manager::new("laptop-a", None);
        let local_a = LocalNode {
            machine: "laptop-a".into(),
            label: Some("Laptop A".into()),
            host: None,
            waiters: Arc::default(),
            reach: Some(handle_a),
            convo: None,
            convo_raft: Some(Arc::clone(&raft_a)),
        };
        let door_a = Arc::new(Door::for_node(Upstream::parse(&up_url, &[])?, local_a)?);
        let at_a = serve(door_a)?;

        // 1. Check /healthz and /healthz/server on laptop-a's local HTTP server.
        let (st, _, h_body) = get(at_a, "/healthz/server", None)?;
        assert_eq!(st, 200, "{h_body}");
        assert!(h_body.contains("\"role\":\"runner+server\"") && h_body.contains("\"machine\":\"laptop-a\""), "{h_body}");

        // 2. Submit a message to laptop-a targeting laptop-b (with owner cookie + x-kiln-target).
        let mut s = std::net::TcpStream::connect(at_a)?;
        s.set_read_timeout(Some(Duration::from_secs(10)))?;
        let payload = r#"{"type":"send","id":"m-quic-1","text":"hello laptop-b","mode":"queue"}"#;
        write!(
            s,
            "POST /api/harness/sessions/sess-b/command HTTP/1.1\r\nhost: localhost:7070\r\ncookie: kiln_session=owner\r\nx-kiln-target: agent:laptop-b\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
            payload.len()
        )?;
        let mut raw = String::new();
        s.read_to_string(&mut raw)?;
        let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
        let head_lc = head.to_ascii_lowercase();
        let status = head.split_whitespace().nth(1).and_then(|v| v.parse::<u16>().ok()).unwrap_or(0);
        assert_eq!(status, 200, "{head}\n{body}");
        assert!(head_lc.contains("kiln-routed: quic laptop-b"), "expected direct hyper-quic routing header: {head}");
        assert!(body.contains("\"message\":\"m-quic-1\""), "{body}");
        assert_eq!(b_received.load(Ordering::SeqCst), 1, "laptop-b must receive the message once over hyper-quic");
        assert!(a_send_rx.try_recv().is_err(), "laptop-a must NOT enqueue a remote session's message locally");
        Ok(())
    }
}
