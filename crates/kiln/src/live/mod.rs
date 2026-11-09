//! The browser protocol over one WebSocket (goal 3 of the browser protocol program, plan
//! `01a1a832eb2f711f9c6f51d88effe89d`; docs/browser-protocol.md, "The WebSocket carrier"). One
//! endpoint, `GET /api/live`, beside today's sockets (which stay as they are), carries goal 2's
//! protocol in binary messages: its requests, answers and live subscriptions, for every tab of a
//! browser at once (`ui/src/api/bp/`).
//!
//! - **The handshake** (step 2): an HTTP/1.1 upgrade with the session cookie as every request has
//!   (no credentials: 401), an `Origin` of the server's own (else 403: a page elsewhere can't open a
//!   socket that acts as the person), and a `Sec-WebSocket-Protocol` naming a version of the
//!   protocol (`kiln-bp.<n>`; none this server speaks: 400 naming those it does). The 101 names the
//!   version taken and never an extension: permessage-deflate is declined (RFC 7692's own warning of
//!   history-based compression over a secure transport; goal 2's compression is by trust class).
//!   Inside, the page opens with a single-use ticket (`POST /api/live/ticket`) minted for the same
//!   caller.
//! - **The send path** (step 3, [`send`]): what waits, waits in the scheduler; below it a message of
//!   at most a chunk, one at a time, and a low-water mark sized from the path.
//! - **The window** (step 4, `kiln_bp::window`): no more than a window of bytes the page hasn't said
//!   it has, sized from its acknowledgements; and BBR on the socket where `[server]
//!   browser_congestion` asks for it and the host allows it (off until declared).
//! - **The topics** ([`owner`]): one task makes the hub's events into topic changes once.
//!
//! Off until declared: `[server] live = true` (or `KILN_LIVE=1`) serves the endpoint; until then it
//! is not found, and `/api/me` says nothing of it. Ada's call (the plan's open questions): goal 9
//! moves the UI onto it.

pub mod owner;
pub mod send;

mod conn;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use http::{HeaderValue, StatusCode, header};
use kiln_bp::frames::{OLDEST, VERSION};
use kiln_bp::tickets::{Principal, TICKET_LIFE_MS, Tickets};
use serde_json::{Value, json};

use crate::http::server::{self as srv, Request, Response};
use crate::http::ws;
use crate::hub::Hub;
use crate::rcu::Rcu;

/// Format: the `Sec-WebSocket-Protocol` token of a protocol version: `kiln-bp.<n>`.
pub const PROTOCOL: &str = "kiln-bp.";

/// The path of the endpoint.
pub const PATH: &str = "/api/live";

/// Shape: the largest message the page may send up: the protocol's largest frame whole (1 MiB) and
/// room for its envelope.
const MAX_UP: usize = (1 << 20) + (64 << 10);

/// Whether this process serves the endpoint (`[server] live`): read where there's no hub (the HTTP
/// server's accept, which makes a socket's options handle only when it may be needed).
static ENABLED: AtomicBool = AtomicBool::new(false);

/// The token of version `v`.
pub fn token(v: u32) -> String {
    format!("{PROTOCOL}{v}")
}

/// The versions this build speaks, newest first, as tokens.
pub fn versions() -> Vec<String> {
    (OLDEST..=VERSION).rev().map(token).collect()
}

/// The newest version the page offers (`Sec-WebSocket-Protocol`, a list) that this build speaks.
pub fn chosen(offered: &str) -> Option<u32> {
    offered
        .split(',')
        .filter_map(|t| t.trim().strip_prefix(PROTOCOL))
        .filter_map(|v| v.parse::<u32>().ok())
        .filter(|v| (OLDEST..=VERSION).contains(v))
        .max()
}

/// What an HTTP/1.1 connection carries for the protocol's socket, should one of its requests be an
/// upgrade to it: a handle to its socket's options (`hyper_rt::tcp::SocketOptions`, `None` where the
/// platform makes none), the bound its TLS session keeps on what it buffers to send (0: TLS's own,
/// 64 KiB), which the socket sets to its chunk, with what TLS holds (`crate::http::tls`), and what
/// tungstenite holds ([`WsLayer`]). In the requests' extensions.
#[derive(Debug, Default)]
pub struct Control {
    pub socket: Option<hyper_rt::tcp::SocketOptions>,
    pub tls: Arc<crate::http::tls::SendBound>,
    pub ws: Arc<WsLayer>,
}

/// What tungstenite holds of the protocol socket's messages (step 3: a counter at each layer below
/// the scheduler): the bytes handed to it (each message's frame whole, from the socket's start), the
/// bytes the layer below it took (TLS, or the kernel without TLS: [`Counted`]), and the most it held
/// when it gave the layer below bytes. Written by the socket's task, read by anyone, never locked. A
/// pong tungstenite writes by itself counts as taken and not handed: what's held reads low by its
/// few bytes, never high.
#[derive(Debug, Default)]
pub struct WsLayer {
    pub handed: AtomicU64,
    pub taken: AtomicU64,
    pub held_max: AtomicU64,
}

impl WsLayer {
    /// From now on: what the connection wrote before (its HTTP answers, the 101) isn't tungstenite's.
    pub fn start(&self) {
        self.handed.store(self.taken.load(Ordering::Relaxed), Ordering::Relaxed);
    }

    /// A message of `payload` bytes handed to tungstenite: its frame whole (a server's: unmasked).
    pub fn hand(&self, payload: usize) {
        let header = match payload {
            0..=125 => 2,
            126..=65_535 => 4,
            _ => 10,
        };
        self.handed.fetch_add(u64::try_from(payload.saturating_add(header)).unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    /// The bytes tungstenite holds now.
    pub fn held(&self) -> u64 {
        self.handed.load(Ordering::Relaxed).saturating_sub(self.taken.load(Ordering::Relaxed))
    }
}

/// An HTTP/1.1 connection's IO as its requests (and a WebSocket upgraded from one) write to it,
/// counting into [`WsLayer`] what the layer below takes, when the protocol is served (`None`: as
/// it is).
pub struct Counted<I> {
    io: I,
    ws: Option<Arc<WsLayer>>,
}

impl<I> Counted<I> {
    pub fn new(io: I, ws: Option<Arc<WsLayer>>) -> Self {
        Self { io, ws }
    }

    fn took(&self, n: usize) {
        if let Some(w) = &self.ws {
            w.held_max.fetch_max(w.held(), Ordering::Relaxed);
            w.taken.fetch_add(u64::try_from(n).unwrap_or(u64::MAX), Ordering::Relaxed);
        }
    }
}

impl<I: futures::AsyncRead + Unpin> futures::AsyncRead for Counted<I> {
    fn poll_read(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &mut [u8]) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().io).poll_read(cx, buf)
    }
}

impl<I: futures::AsyncWrite + Unpin> futures::AsyncWrite for Counted<I> {
    fn poll_write(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &[u8]) -> std::task::Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let r = std::pin::Pin::new(&mut this.io).poll_write(cx, buf);
        if let std::task::Poll::Ready(Ok(n)) = r {
            this.took(n);
        }
        r
    }

    fn poll_write_vectored(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, bufs: &[std::io::IoSlice<'_>]) -> std::task::Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let r = std::pin::Pin::new(&mut this.io).poll_write_vectored(cx, bufs);
        if let std::task::Poll::Ready(Ok(n)) = r {
            this.took(n);
        }
        r
    }

    fn poll_flush(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().io).poll_flush(cx)
    }

    fn poll_close(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().io).poll_close(cx)
    }
}

/// A connection's [`Control`], when this process serves the endpoint (a second descriptor of the
/// socket otherwise costs every connection one).
pub fn control_for(stream: &hyper_rt::tcp::TcpStream) -> Option<Arc<Control>> {
    if !ENABLED.load(Ordering::Relaxed) {
        return None;
    }
    let socket = stream.options().ok().flatten();
    Some(Arc::new(Control { socket, ..Control::default() }))
}

/// One protocol socket, as `/metrics` and `GET /api/live/sockets` show it: each number set by its
/// own task, read by anyone, never locked.
#[derive(Debug, Default)]
pub struct Stats {
    pub id: u64,
    /// Who opened it (as the audit log names them), and which browser session (a digest of its
    /// cookie: the same for every tab of one browser).
    pub who: String,
    pub session: String,
    pub opened_ms: i64,
    /// The kernel's: unsent bytes, bytes in flight (Linux `TCP_INFO`).
    pub unsent: AtomicU64,
    pub in_flight: AtomicU64,
    /// The low-water mark and the chunk in use, and the largest of each the socket has used (what the
    /// kernel and the layers above it were let take when they took it).
    pub mark: AtomicU64,
    pub chunk: AtomicU64,
    pub mark_max: AtomicU64,
    pub chunk_max: AtomicU64,
    /// The window, and the bytes sent the page hasn't said it has.
    pub window: AtomicU64,
    pub unacked: AtomicU64,
    /// The most `unacked` has been past the window in effect when it was sent (bytes; 0: never).
    pub over_window: AtomicU64,
    /// The bytes below the scheduler in user space now: the message being written (tungstenite's
    /// buffer and TLS's together, flushed before the next is given).
    pub held: AtomicU64,
    pub held_max: AtomicU64,
    /// The most each layer held, by its own counter: tungstenite ([`WsLayer`]) and TLS
    /// (`crate::http::tls::SendBound`; 0 without TLS).
    pub held_ws_max: AtomicU64,
    pub held_tls_max: AtomicU64,
    /// What waits in the scheduler, by priority (control, interactive, state, stream, bulk).
    pub queued: [AtomicU64; 5],
    /// The path as measured: bytes a second, and the least round trip (ms).
    pub rate: AtomicU64,
    pub min_rtt_ms: AtomicU64,
    /// Bytes the kernel retransmitted.
    pub retransmitted: AtomicU64,
    pub sent: AtomicU64,
    pub received: AtomicU64,
    /// The congestion control the socket has (what was asked, and what came of it).
    pub congestion: OnceLock<String>,
}

impl Stats {
    pub fn json(&self) -> Value {
        let n = |a: &AtomicU64| a.load(Ordering::Relaxed);
        json!({
            "id": self.id, "who": self.who, "session": self.session, "opened_ms": self.opened_ms,
            "unsent": n(&self.unsent), "in_flight": n(&self.in_flight), "mark": n(&self.mark), "chunk": n(&self.chunk), "mark_max": n(&self.mark_max), "chunk_max": n(&self.chunk_max),
            "window": n(&self.window), "unacked": n(&self.unacked), "over_window": n(&self.over_window),
            "held": n(&self.held), "held_max": n(&self.held_max), "held_ws_max": n(&self.held_ws_max), "held_tls_max": n(&self.held_tls_max),
            "queued": self.queued.iter().map(n).collect::<Vec<u64>>(),
            "rate": n(&self.rate), "min_rtt_ms": n(&self.min_rtt_ms), "retransmitted": n(&self.retransmitted),
            "sent": n(&self.sent), "received": n(&self.received),
            "congestion": self.congestion.get(),
        })
    }
}

/// The process-wide counts of the protocol's sockets (`kiln_proto_*`).
#[derive(Default)]
pub struct Metrics {
    pub opened: crate::metrics::Counter,
    pub refused: crate::metrics::Counter,
    pub events_dropped: crate::metrics::Counter,
    pub tickets: crate::metrics::Counter,
}

pub fn metrics() -> &'static Metrics {
    static M: OnceLock<Metrics> = OnceLock::new();
    M.get_or_init(Metrics::default)
}

/// The protocol sockets this process holds, by id: written as one opens or ends, read by `/metrics`
/// and `GET /api/live/sockets`.
fn registry() -> &'static Rcu<BTreeMap<u64, Arc<Stats>>> {
    static R: OnceLock<Rcu<BTreeMap<u64, Arc<Stats>>>> = OnceLock::new();
    R.get_or_init(Rcu::default)
}

/// The sockets held now.
pub fn held() -> Vec<Arc<Stats>> {
    registry().load().values().cloned().collect()
}

/// A server's protocol state: its tickets, its topics' owner and its sockets.
pub struct Live {
    tickets: Rcu<Tickets>,
    owner: OnceLock<owner::Handle>,
    next: AtomicU64,
    /// The server's incarnation: its topics' cursors are of it.
    born: u64,
    enabled: bool,
    /// `[server] browser_congestion`, checked at start: what the sockets ask for.
    congestion: Option<hyper_rt::tcp::CongestionName>,
    /// `[server] browser_window`: whether the sockets keep to their end-to-end window.
    window: bool,
}

impl std::fmt::Debug for Live {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Live").field("enabled", &self.enabled).field("born", &self.born).finish_non_exhaustive()
    }
}

impl Live {
    /// A server's, served as `cfg` says.
    pub fn new(cfg: &crate::config::Server) -> Self {
        if cfg.live {
            ENABLED.store(true, Ordering::Relaxed);
        }
        let congestion = cfg.browser_congestion.as_deref().map(str::trim).filter(|c| !c.is_empty()).and_then(hyper_rt::tcp::CongestionName::new);
        Self {
            tickets: Rcu::new(Tickets::new()),
            owner: OnceLock::new(),
            next: AtomicU64::new(1),
            born: u64::try_from(crate::util::now_ms()).unwrap_or(0),
            enabled: cfg.live,
            congestion,
            window: cfg.browser_window,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// One of the hub's events, for the topics: nothing until a socket has started the owner.
    pub fn event(&self, e: &crate::wire::events::ServerEvent) {
        if let Some(o) = self.owner.get() {
            o.event(e);
        }
    }

    /// The topics' owner, started with the first socket.
    fn owner(&self) -> &owner::Handle {
        self.owner.get_or_init(|| owner::Handle::start(self.born))
    }


    /// What `/api/me` and `/api/system` say of the endpoint, when it's served.
    pub fn offer(&self) -> Value {
        if !self.enabled {
            return Value::Null;
        }
        json!({ "path": PATH, "ticket": format!("{PATH}/ticket"), "versions": versions(), "congestion": self.congestion.map(|c| c.to_string()), "window": self.window })
    }
}

/// A digest of the browser session's cookie: the same for every tab of one browser, and nothing an
/// eavesdropper on `/metrics` could sign in with.
fn session_of(req: &Request) -> (String, Option<u64>) {
    let cookie = req
        .headers()
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == "kiln_session")
        .map(|(_, v)| v.to_owned());
    match cookie {
        Some(c) => {
            // The owner's cookie is `<expiry s>.<signature>`: the connection is cut when it expires.
            let expires = c.split_once('.').and_then(|(e, _)| e.parse::<u64>().ok()).map(|s| s.saturating_mul(1_000));
            let digest = crate::util::sha256_hex(c.as_bytes());
            (digest.get(..16).unwrap_or(&digest).to_owned(), expires)
        }
        // A token, not a browser: each socket a session of its own.
        None => (String::new(), None),
    }
}

/// `POST /api/live/ticket`: a single-use ticket for the caller, good for one open within its life
/// (docs/browser-protocol.md, "The ticket").
pub fn ticket(hub: &Hub, caller: &crate::authz::Caller, req: &Request) -> Response {
    if !hub.live.enabled {
        return srv::text(StatusCode::NOT_FOUND, "not found");
    }
    use base64::Engine as _;
    let (session, expires_at) = session_of(req);
    let now = u64::try_from(crate::util::now_ms()).unwrap_or(0);
    let random: [u8; 32] = rand::random();
    let principal = Principal { who: caller.name.clone(), session, expires_at };
    let t = hub.live.tickets.update(|t| t.mint(random, principal.clone(), now));
    metrics().tickets.inc();
    srv::json(
        StatusCode::OK,
        &json!({ "ticket": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(t), "life_ms": TICKET_LIFE_MS, "versions": versions(), "path": PATH }),
    )
}

/// `GET /api/live/sockets`: the caller's own protocol sockets (every one, for the owner).
pub fn sockets(hub: &Hub, caller: &crate::authz::Caller) -> Response {
    let _ = hub;
    let list: Vec<Value> = held().iter().filter(|s| caller.is_owner() || s.who == caller.name).map(|s| s.json()).collect();
    srv::json(StatusCode::OK, &json!({ "sockets": list }))
}

/// Whether the request's `Origin` is one of the server's own: the address it was reached at (its
/// `Host`), its public URL, or a front door's name. No `Origin` is a client that isn't a browser
/// (it can say anything anyway); a browser always sends one with a WebSocket.
pub fn origin_ok(hub: &Hub, req: &Request) -> bool {
    let Some(origin) = req.headers().get(header::ORIGIN).and_then(|v| v.to_str().ok()) else { return true };
    let origin = origin.trim().trim_end_matches('/');
    let Some((scheme, authority)) = origin.split_once("://") else { return false };
    if !matches!(scheme, "http" | "https") {
        return false;
    }
    if req.headers().get(header::HOST).and_then(|v| v.to_str().ok()).is_some_and(|h| h.trim().eq_ignore_ascii_case(authority)) {
        return true;
    }
    let ours = std::iter::once(hub.cfg.server.public_url.clone()).chain(hub.front_doors.origins());
    ours.into_iter().any(|o| o.trim_end_matches('/').eq_ignore_ascii_case(origin))
}

/// The lab's page (`KILN_LIVE_LAB`, a folder): served at `/__live/<file>` on the server's own origin, as
/// the shared socket's worker must be, for the lab and the browser tests (`ui/e2e/bp-shared.spec.ts`).
/// Unset (every server but a lab's), there is no such path.
pub fn lab_dir() -> Option<std::path::PathBuf> {
    static D: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    D.get_or_init(|| std::env::var_os("KILN_LIVE_LAB").map(std::path::PathBuf::from).filter(|p| p.is_dir())).clone()
}

/// `GET /__live/<file>`: one of the lab's files, by a plain name (letters, digits, `.`, `-`, `_`).
pub async fn lab_file(name: &str) -> Response {
    let Some(dir) = lab_dir() else { return srv::text(StatusCode::NOT_FOUND, "not found") };
    if name.is_empty() || name.starts_with('.') || !name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_')) {
        return srv::text(StatusCode::NOT_FOUND, "not found");
    }
    let path = dir.join(name);
    let kind = if name.ends_with(".html") { "text/html; charset=utf-8" } else if name.ends_with(".js") { "text/javascript" } else { "application/octet-stream" };
    match crate::rt::blocking(move || std::fs::read(path)).await {
        Ok(Ok(bytes)) => srv::with_header(srv::response(StatusCode::OK, kind, srv::full(bytes)), header::CACHE_CONTROL, "no-store"),
        _ => srv::text(StatusCode::NOT_FOUND, "not found"),
    }
}

/// A refusal of the handshake, as JSON naming why (and, for a version, those this server speaks).
fn refuse(status: StatusCode, why: &str, with_versions: bool) -> Response {
    metrics().refused.inc();
    let mut body = json!({ "error": why });
    if with_versions && let Some(o) = body.as_object_mut() {
        o.insert("versions".into(), json!(versions()));
    }
    let mut r = srv::json(status, &body);
    if with_versions && let Ok(v) = HeaderValue::from_str(&versions().join(", ")) {
        r.headers_mut().insert(header::SEC_WEBSOCKET_PROTOCOL, v);
    }
    r
}

/// `GET /api/live`: the upgrade, checked (Origin, version), then the socket's task.
pub fn socket(hub: Arc<Hub>, caller: crate::authz::Caller, mut req: Request) -> Response {
    if !hub.live.enabled {
        return srv::text(StatusCode::NOT_FOUND, "not found");
    }
    if !ws::is_upgrade(&req) {
        return refuse(StatusCode::BAD_REQUEST, "expected a WebSocket upgrade", false);
    }
    if !origin_ok(&hub, &req) {
        return refuse(StatusCode::FORBIDDEN, "this page's origin isn't the server's own", false);
    }
    let offered = req.headers().get_all(header::SEC_WEBSOCKET_PROTOCOL).iter().filter_map(|v| v.to_str().ok()).collect::<Vec<_>>().join(",");
    let Some(version) = chosen(&offered) else {
        return refuse(StatusCode::BAD_REQUEST, "no protocol version this server speaks: it offers those in Sec-WebSocket-Protocol", true);
    };
    let (session, _) = session_of(&req);
    let control = req.extensions_mut().remove::<Arc<Control>>();
    let (mut resp, upgrade) = match ws::accept_with(req, conn::ws_config(MAX_UP)) {
        Ok(a) => a,
        Err(r) => return refuse(r.status, r.reason, false),
    };
    if let Ok(v) = HeaderValue::from_str(&token(version)) {
        resp.headers_mut().insert(header::SEC_WEBSOCKET_PROTOCOL, v);
    }
    let id = hub.live.next.fetch_add(1, Ordering::Relaxed);
    let stats = Arc::new(Stats { id, who: caller.name.clone(), session, opened_ms: crate::util::now_ms(), ..Stats::default() });
    let owner = hub.live.owner().clone();
    crate::rt::spawn(async move {
        let Ok(socket) = upgrade.await else { return };
        metrics().opened.inc();
        registry().update(|m| {
            m.insert(id, Arc::clone(&stats));
        });
        let ended = Box::pin(conn::run(conn::Socket { hub: Arc::clone(&hub), caller, owner, socket, control, stats, version })).await;
        if let Err(e) = ended {
            tracing::debug!(error = %e, id, "a protocol socket ended");
        }
        registry().update(|m| {
            m.remove(&id);
        });
    });
    resp
}

/// The `kiln_proto_*` families: the process's counts, and each socket's numbers by its id.
pub fn render(o: &mut String) {
    use std::fmt::Write as _;
    let m = metrics();
    let _ = writeln!(o, "# HELP kiln_proto_sockets_opened_total Browser protocol sockets opened (goal 3, /api/live).\n# TYPE kiln_proto_sockets_opened_total counter\nkiln_proto_sockets_opened_total {}", m.opened.get());
    let _ = writeln!(o, "# HELP kiln_proto_handshakes_refused_total Browser protocol upgrades refused (origin, version, not an upgrade).\n# TYPE kiln_proto_handshakes_refused_total counter\nkiln_proto_handshakes_refused_total {}", m.refused.get());
    let _ = writeln!(o, "# HELP kiln_proto_tickets_total Browser protocol tickets minted.\n# TYPE kiln_proto_tickets_total counter\nkiln_proto_tickets_total {}", m.tickets.get());
    let _ = writeln!(o, "# HELP kiln_proto_events_dropped_total Hub events the topics' owner had no room for.\n# TYPE kiln_proto_events_dropped_total counter\nkiln_proto_events_dropped_total {}", m.events_dropped.get());
    let sockets = held();
    let _ = writeln!(o, "# HELP kiln_proto_sockets Browser protocol sockets held now.\n# TYPE kiln_proto_sockets gauge\nkiln_proto_sockets {}", sockets.len());
    let gauges: [(&str, &str, fn(&Stats) -> u64); 11] = [
        ("kiln_proto_unsent_bytes", "Bytes the kernel holds unsent, per socket (Linux TCP_INFO).", |s| s.unsent.load(Ordering::Relaxed)),
        ("kiln_proto_in_flight_bytes", "Bytes in flight, per socket (Linux TCP_INFO).", |s| s.in_flight.load(Ordering::Relaxed)),
        ("kiln_proto_mark_bytes", "The low-water mark in use (TCP_NOTSENT_LOWAT), per socket.", |s| s.mark.load(Ordering::Relaxed)),
        ("kiln_proto_chunk_bytes", "The chunk in use (the most a message holds), per socket.", |s| s.chunk.load(Ordering::Relaxed)),
        ("kiln_proto_window_bytes", "The end-to-end window, per socket.", |s| s.window.load(Ordering::Relaxed)),
        ("kiln_proto_unacked_bytes", "Bytes sent the page hasn't said it has, per socket.", |s| s.unacked.load(Ordering::Relaxed)),
        ("kiln_proto_held_bytes", "Bytes below the scheduler in user space (the message being written), per socket.", |s| s.held.load(Ordering::Relaxed)),
        ("kiln_proto_rate_bytes_per_second", "The path's rate as measured, per socket.", |s| s.rate.load(Ordering::Relaxed)),
        ("kiln_proto_min_rtt_ms", "The path's least round trip as measured, per socket.", |s| s.min_rtt_ms.load(Ordering::Relaxed)),
        ("kiln_proto_sent_bytes", "Bytes sent, per socket.", |s| s.sent.load(Ordering::Relaxed)),
        ("kiln_proto_retransmitted_bytes", "Bytes the kernel retransmitted, per socket (Linux).", |s| s.retransmitted.load(Ordering::Relaxed)),
    ];
    for (name, help, get) in gauges {
        let _ = writeln!(o, "# HELP {name} {help}\n# TYPE {name} gauge");
        for s in &sockets {
            let _ = writeln!(o, "{name}{{socket=\"{}\"}} {}", s.id, get(s));
        }
    }
    let _ = writeln!(o, "# HELP kiln_proto_held_max_bytes The most a layer below the scheduler held, per socket and layer (tungstenite, TLS).\n# TYPE kiln_proto_held_max_bytes gauge");
    for s in &sockets {
        for (layer, n) in [("ws", &s.held_ws_max), ("tls", &s.held_tls_max)] {
            let _ = writeln!(o, "kiln_proto_held_max_bytes{{socket=\"{}\",layer=\"{layer}\"}} {}", s.id, n.load(Ordering::Relaxed));
        }
    }
    let _ = writeln!(o, "# HELP kiln_proto_queued_bytes What waits in the scheduler, per socket and priority.\n# TYPE kiln_proto_queued_bytes gauge");
    for s in &sockets {
        for (p, q) in kiln_bp::conn::PRIOS.iter().zip(s.queued.iter()) {
            let _ = writeln!(o, "kiln_proto_queued_bytes{{socket=\"{}\",priority=\"{}\"}} {}", s.id, p.name(), q.load(Ordering::Relaxed));
        }
    }
}

#[cfg(test)]
mod tests;
