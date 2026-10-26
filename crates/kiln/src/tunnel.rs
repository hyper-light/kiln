//! The HTTP tunnel: a runner whose UDP to the server's QUIC port is dropped (a firewall that lets
//! only the HTTP port through, say) carries its QUIC datagrams in a WebSocket on the HTTP port.
//!
//! ```text
//!   agent QUIC ─UDP─▶ relay (127.0.0.1:0) ═WS GET /api/agents/tunnel═▶ server relay ─UDP─▶ QUIC listener
//! ```
//!
//! QUIC and its mutual TLS run end to end through it, unchanged: the agent still verifies the
//! server by the name [`crate::pki::SERVER_NAME`] (not by address, so dialing a loopback relay
//! loosens nothing) and the server the agent's certificate. So the route is open at the HTTP level
//! and bounded instead: [`MAX_TUNNELS`] at once, [`OPENS_PER_MINUTE`] new ones per source address,
//! one datagram of at most [`MAX_DATAGRAM`] bytes a message, closed after [`IDLE`] without traffic.
//!
//! One binary message is one datagram, either way. The agent's text message `bind <hex>` names the
//! TLS exporter value of the QUIC connection the tunnel carries ([`crate::net::BINDING_LABEL`]),
//! which only the two ends of that connection know: the server matches it against its own
//! connections' to report which runners are tunnelled (`transport` in `GET /api/agents`).
//!
//! An agent ([`Paths`]) dials UDP first, takes the tunnel after [`DIRECT_TRIES`] direct dials in a
//! row get no answer (or [`DIRECT_PATIENCE`] of them), and while tunnelled checks every
//! [`PROBE_EVERY`] whether UDP answers again ([`udp_answers`]), switching back when it does. It
//! remembers which path it last connected by ([`Paths::remembering`]): on a network that drops
//! UDP, a restart or an upgrade takes the tunnel at once instead of waiting for UDP again.

use std::collections::HashMap;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use async_tungstenite::{WebSocketReceiver, WebSocketSender};
use futures::io::{AsyncRead, AsyncWrite};
use futures::{FutureExt as _, StreamExt as _};
use http::StatusCode;
use hyper_rt::udp::UdpSocket;
use hyper_transport::PeerId;
use parking_lot::Mutex;
use serde::Serialize;

use crate::http::client::Client;
use crate::http::server::{Request, Response};
use crate::http::ws::{self, Message};

/// The tunnel's route on the server's HTTP port.
pub const PATH: &str = "/api/agents/tunnel";
/// The largest datagram a tunnel carries, and so the largest message (QUIC's own datagrams stay
/// at or under 1452 bytes: its path MTU discovery stops there).
pub const MAX_DATAGRAM: usize = 1500;
/// Tunnels a server holds open at once.
pub const MAX_TUNNELS: usize = 256;
/// New tunnels a server accepts from one source address a minute.
pub const OPENS_PER_MINUTE: u32 = 60;
/// A tunnel that relays nothing for this long is closed (QUIC keeps a live one busier than that).
pub const IDLE: Duration = Duration::from_secs(600);
/// Direct dials in a row that get no answer before an agent takes the tunnel...
pub const DIRECT_TRIES: u32 = 2;
/// ...or how long its direct dials may go unanswered.
pub const DIRECT_PATIENCE: Duration = Duration::from_secs(20);
/// While tunnelled, how often an agent checks whether UDP answers again.
pub const PROBE_EVERY: Duration = Duration::from_secs(300);
/// How long one check waits for an answer.
const PROBE_WAIT: Duration = Duration::from_secs(3);
const MINUTE: Duration = Duration::from_secs(60);

/// How a runner's QUIC connection reaches the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    /// UDP straight to the QUIC listener.
    Direct,
    /// Through the HTTP tunnel.
    Tunnel,
}

impl fmt::Display for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Direct => "direct",
            Self::Tunnel => "tunnel",
        })
    }
}

// ------------------------------------------------------------------ framing

/// What one WebSocket message on a tunnel carries.
#[derive(Debug, PartialEq, Eq)]
enum Inbound<'a> {
    /// One datagram.
    Datagram(&'a [u8]),
    /// The binding of the QUIC connection the tunnel carries (text `bind <64 hex digits>`).
    Bind([u8; 32]),
    /// The other end is closing.
    Close,
    /// Nothing to relay: pings and pongs (tungstenite answers pings itself), an empty or
    /// oversized datagram, text that is not a binding.
    Skip,
}

fn inbound(msg: &Message) -> Inbound<'_> {
    match msg {
        Message::Binary(b) if !b.is_empty() && b.len() <= MAX_DATAGRAM => Inbound::Datagram(b),
        Message::Text(t) => parse_bind(t.as_str()).map_or(Inbound::Skip, Inbound::Bind),
        Message::Close(_) => Inbound::Close,
        Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => Inbound::Skip,
    }
}

/// One datagram as a message; `None` for an empty one or one larger than a tunnel carries (it is
/// dropped, as a link with a smaller MTU would).
fn outbound(datagram: &[u8]) -> Option<Message> {
    (!datagram.is_empty() && datagram.len() <= MAX_DATAGRAM).then(|| ws::binary(datagram.to_vec()))
}

fn bind_text(binding: &[u8; 32]) -> Message {
    ws::text(format!("bind {}", hex::encode(binding)))
}

fn parse_bind(text: &str) -> Option<[u8; 32]> {
    let digits = text.strip_prefix("bind ")?;
    <[u8; 32]>::try_from(hex::decode(digits.trim()).ok()?.as_slice()).ok()
}

/// Send one datagram without waiting: a socket with no room for it drops it, as a congested link
/// would (QUIC recovers either way).
fn send_datagram(udp: &UdpSocket, datagram: &[u8], to: SocketAddr) {
    if let Err(e) = udp.try_send_to(datagram, to) {
        tracing::debug!(error = %e, %to, "datagram not relayed");
    }
}

/// Relay a tunnel's messages to UDP: each datagram to `to()` (none known yet: dropped), each
/// binding to `on_bind`. Returns why the tunnel ended.
async fn ws_to_udp<S>(
    rx: &mut WebSocketReceiver<S>,
    udp: &UdpSocket,
    to: impl Fn() -> Option<SocketAddr>,
    on_bind: impl Fn([u8; 32]),
    activity: &Activity,
) -> anyhow::Result<&'static str>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    while let Some(msg) = rx.next().await {
        match inbound(&msg?) {
            Inbound::Datagram(d) => {
                activity.touch();
                if let Some(to) = to() {
                    send_datagram(udp, d, to);
                }
            }
            Inbound::Bind(b) => on_bind(b),
            Inbound::Close => return Ok("closed by the other end"),
            Inbound::Skip => {}
        }
    }
    Ok("the other end went away")
}

/// When a tunnel last carried something.
struct Activity {
    start: Instant,
    last_ms: AtomicU64,
}

impl Activity {
    fn new() -> Self {
        Self { start: Instant::now(), last_ms: AtomicU64::new(0) }
    }

    fn touch(&self) {
        let ms = u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_ms.store(ms, Ordering::Relaxed);
    }

    /// Resolves once nothing has been carried for `limit`.
    async fn idle(&self, limit: Duration) {
        loop {
            let last = self.start + Duration::from_millis(self.last_ms.load(Ordering::Relaxed));
            let due = last + limit;
            if Instant::now() >= due {
                return;
            }
            crate::rt::sleep_until(due).await;
        }
    }
}

// ------------------------------------------------------------------ server

/// The server's live tunnels and what each carries, and its QUIC connections' bindings.
#[derive(Default)]
pub struct Registry {
    inner: Mutex<RegistryInner>,
}

#[derive(Default)]
struct RegistryInner {
    next: u64,
    /// Live tunnels, each with the binding it last named.
    live: HashMap<u64, Option<[u8; 32]>>,
    /// New tunnels per source address in the current minute: (minute start, count).
    opens: HashMap<IpAddr, (Instant, u32)>,
    /// Each connected peer's binding, as the QUIC listener derived it.
    peers: HashMap<PeerId, [u8; 32]>,
}

/// Why a tunnel was not accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// [`MAX_TUNNELS`] are open.
    Full,
    /// Its address opened [`OPENS_PER_MINUTE`] this minute.
    TooOften,
}

impl Registry {
    /// Admit one more tunnel from `from` (when known); its [`Slot`] holds the place until dropped.
    pub fn admit(self: &Arc<Self>, from: Option<IpAddr>, now: Instant) -> Result<Slot, Refusal> {
        let mut g = self.inner.lock();
        if g.live.len() >= MAX_TUNNELS {
            return Err(Refusal::Full);
        }
        if let Some(ip) = from {
            if g.opens.len() >= 4096 {
                g.opens.retain(|_, (start, _)| now.saturating_duration_since(*start) < MINUTE);
            }
            let (start, count) = g.opens.entry(ip).or_insert((now, 0));
            if now.saturating_duration_since(*start) >= MINUTE {
                (*start, *count) = (now, 0);
            }
            if *count >= OPENS_PER_MINUTE {
                return Err(Refusal::TooOften);
            }
            *count = count.saturating_add(1);
        }
        g.next = g.next.saturating_add(1);
        let id = g.next;
        g.live.insert(id, None);
        Ok(Slot { registry: Arc::clone(self), id })
    }

    /// A QUIC connection from `peer` is up, with its binding.
    pub fn connected(&self, peer: PeerId, binding: Option<[u8; 32]>) {
        let mut g = self.inner.lock();
        match binding {
            Some(b) => g.peers.insert(peer, b),
            None => g.peers.remove(&peer),
        };
    }

    /// `peer`'s connection is gone.
    pub fn closed(&self, peer: PeerId) {
        self.inner.lock().peers.remove(&peer);
    }

    /// How `peer`'s connection reaches the listener: through a tunnel that named its binding, or
    /// directly.
    pub fn transport(&self, peer: PeerId) -> Transport {
        let g = self.inner.lock();
        match g.peers.get(&peer) {
            Some(b) if g.live.values().any(|t| t.as_ref() == Some(b)) => Transport::Tunnel,
            _ => Transport::Direct,
        }
    }

    /// Tunnels open now.
    pub fn open(&self) -> usize {
        self.inner.lock().live.len()
    }
}

/// A live tunnel's place in the [`Registry`]; frees it, and its binding, when dropped.
pub struct Slot {
    registry: Arc<Registry>,
    id: u64,
}

impl Slot {
    fn bind(&self, binding: [u8; 32]) {
        if let Some(b) = self.registry.inner.lock().live.get_mut(&self.id) {
            *b = Some(binding);
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.registry.inner.lock().live.remove(&self.id);
    }
}

/// Where a tunnel delivers its datagrams: the server's own QUIC listener (bound at `quic_bind`) as
/// this host reaches it. A wildcard bind answers on loopback of its family; a specific address only
/// as itself.
pub fn listener_for(quic_bind: SocketAddr) -> SocketAddr {
    match quic_bind {
        SocketAddr::V4(a) if a.ip().is_unspecified() => SocketAddr::from((Ipv4Addr::LOCALHOST, a.port())),
        SocketAddr::V6(a) if a.ip().is_unspecified() => SocketAddr::from((Ipv6Addr::LOCALHOST, a.port())),
        a => a,
    }
}

/// A local address to talk to `to` from: its own address (a local one), any port.
fn local_end(to: SocketAddr) -> SocketAddr {
    match to {
        SocketAddr::V4(a) => SocketAddr::from((*a.ip(), 0)),
        SocketAddr::V6(a) => SocketAddr::V6(SocketAddrV6::new(*a.ip(), 0, 0, a.scope_id())),
    }
}

/// `GET /api/agents/tunnel`: accept a tunnel to the QUIC listener at `listener` (see the module
/// docs). Refusals come back as a status and a reason.
pub fn serve(registry: &Arc<Registry>, listener: SocketAddr, req: Request) -> Result<Response, (StatusCode, &'static str)> {
    if !ws::is_upgrade(&req) {
        return Err((StatusCode::BAD_REQUEST, "expected a WebSocket upgrade"));
    }
    let from = crate::http::server::remote_ip(&req);
    let slot = registry.admit(from, Instant::now()).map_err(|r| match r {
        Refusal::Full => (StatusCode::SERVICE_UNAVAILABLE, "too many tunnels are open"),
        Refusal::TooOften => (StatusCode::TOO_MANY_REQUESTS, "too many tunnels from this address; try again in a minute"),
    })?;
    let (resp, upgrade) = ws::accept_limited(req, MAX_DATAGRAM).map_err(|r| (r.status, r.reason))?;
    let open = registry.open();
    crate::rt::spawn(async move {
        let Ok(socket) = upgrade.await else { return };
        tracing::info!(from = ?from, open, "runner tunnel opened");
        match relay_server(socket, listener, &slot).await {
            Ok(why) => tracing::info!(from = ?from, why, "runner tunnel closed"),
            Err(e) => tracing::info!(from = ?from, error = %format!("{e:#}"), "runner tunnel ended"),
        }
    });
    Ok(resp)
}

/// Relay one tunnel to `listener` until either end closes, an error, or [`IDLE`].
async fn relay_server(socket: ws::WebSocket, listener: SocketAddr, slot: &Slot) -> anyhow::Result<&'static str> {
    let udp = UdpSocket::bind(local_end(listener)).map_err(|e| anyhow::anyhow!("bind a socket toward {listener}: {e}"))?;
    let (mut tx, mut rx) = socket.split();
    let activity = Activity::new();
    let ended = {
        let up = ws_to_udp(&mut rx, &udp, || Some(listener), |b| slot.bind(b), &activity).fuse();
        let down = listener_to_ws(&mut tx, &udp, listener, &activity).fuse();
        let idle = activity.idle(IDLE).fuse();
        futures::pin_mut!(up, down, idle);
        futures::select! {
            r = up => r,
            r = down => r,
            () = idle => Ok("idle"),
        }
    };
    let _ = crate::rt::timeout(Duration::from_secs(2), tx.close(None)).await;
    ended
}

/// The listener's datagrams (only its) back through the tunnel.
async fn listener_to_ws<S>(
    tx: &mut WebSocketSender<S>,
    udp: &UdpSocket,
    listener: SocketAddr,
    activity: &Activity,
) -> anyhow::Result<&'static str>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // One byte more than a tunnel carries, so a longer datagram shows (truncated) and is dropped.
    let mut buf = vec![0u8; MAX_DATAGRAM + 1];
    loop {
        let (n, from) = udp.recv_from(&mut buf).await.map_err(|e| anyhow::anyhow!("receive: {e}"))?;
        if from != listener {
            continue;
        }
        if let Some(msg) = buf.get(..n).and_then(outbound) {
            activity.touch();
            tx.send(msg).await?;
        }
    }
}

// ------------------------------------------------------------------ agent

/// An agent's end of a tunnel: a UDP socket on loopback for its QUIC endpoint to dial, relayed
/// through a WebSocket to the server's HTTP port. Dropping it closes the tunnel.
pub struct Relay {
    addr: SocketAddr,
    bindings: async_channel::Sender<[u8; 32]>,
    alive: Arc<AtomicBool>,
}

impl Relay {
    /// Open a tunnel to the server whose HTTP base URL is `server_url` (`https` gives `wss`).
    pub async fn open(client: &Client, server_url: &str) -> anyhow::Result<Self> {
        let url = format!("{}{PATH}", server_url.trim_end_matches('/'));
        let socket = client.websocket(&url, MAX_DATAGRAM).await?;
        let udp = UdpSocket::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .map_err(|e| anyhow::anyhow!("bind the relay socket: {e}"))?;
        let addr = udp.local_addr().map_err(|e| anyhow::anyhow!("relay socket: {e}"))?;
        let (bindings, bindings_rx) = async_channel::bounded(4);
        let alive = Arc::new(AtomicBool::new(true));
        let flag = Arc::clone(&alive);
        crate::rt::spawn(async move {
            let ended = relay_agent(socket, udp, bindings_rx).await;
            flag.store(false, Ordering::Release);
            match ended {
                Ok(why) => tracing::info!(why, "HTTP tunnel closed"),
                Err(e) => tracing::warn!(error = %format!("{e:#}"), "HTTP tunnel failed"),
            }
        });
        Ok(Self { addr, bindings, alive })
    }

    /// Where the agent's QUIC endpoint dials to go through the tunnel.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Whether the tunnel still relays.
    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    /// Tell the server which QUIC connection the tunnel carries.
    pub fn bind(&self, binding: [u8; 32]) {
        let _ = self.bindings.try_send(binding);
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.bindings.close();
    }
}

/// Relay between the agent's QUIC endpoint and the tunnel until either end closes or an error.
async fn relay_agent(
    socket: ws::ClientSocket,
    udp: UdpSocket,
    bindings: async_channel::Receiver<[u8; 32]>,
) -> anyhow::Result<&'static str> {
    let (mut tx, mut rx) = socket.split();
    // The endpoint's address, learned from what it sends (a rebuilt endpoint has a new port).
    let endpoint: Mutex<Option<SocketAddr>> = Mutex::new(None);
    let activity = Activity::new();
    let ended = {
        let up = endpoint_to_ws(&mut tx, &udp, &bindings, &endpoint).fuse();
        let down = ws_to_udp(&mut rx, &udp, || *endpoint.lock(), |_| {}, &activity).fuse();
        futures::pin_mut!(up, down);
        futures::select! {
            r = up => r,
            r = down => r,
        }
    };
    let _ = crate::rt::timeout(Duration::from_secs(2), tx.close(None)).await;
    ended
}

/// What the agent side sends next.
enum Up {
    Datagram(usize, SocketAddr),
    Bind([u8; 32]),
    /// The [`Relay`] was dropped.
    Done,
}

/// The endpoint's datagrams, and the bindings the agent names, into the tunnel.
async fn endpoint_to_ws<S>(
    tx: &mut WebSocketSender<S>,
    udp: &UdpSocket,
    bindings: &async_channel::Receiver<[u8; 32]>,
    endpoint: &Mutex<Option<SocketAddr>>,
) -> anyhow::Result<&'static str>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; MAX_DATAGRAM + 1];
    loop {
        let next = {
            let datagram = udp.recv_from(&mut buf).fuse();
            let binding = bindings.recv().fuse();
            futures::pin_mut!(datagram, binding);
            futures::select_biased! {
                b = binding => b.map_or(Up::Done, Up::Bind),
                r = datagram => {
                    let (n, from) = r.map_err(|e| anyhow::anyhow!("receive: {e}"))?;
                    Up::Datagram(n, from)
                }
            }
        };
        match next {
            Up::Done => return Ok("closed by this runner"),
            Up::Bind(b) => tx.send(bind_text(&b)).await?,
            Up::Datagram(n, from) => {
                *endpoint.lock() = Some(from);
                if let Some(msg) = buf.get(..n).and_then(outbound) {
                    tx.send(msg).await?;
                }
            }
        }
    }
}

/// Whether UDP reaches the QUIC listener at `quic` and back, without a handshake: a datagram of a
/// reserved version (`0x?a?a?a?a`, RFC 9000 §15) padded to 1200 bytes, which a QUIC server answers
/// with Version Negotiation (§6), is sent about once a second; any answer from that address within
/// `wait` counts.
pub async fn udp_answers(quic: &str, wait: Duration) -> bool {
    let Ok(to) = crate::rt::resolve(quic).await else { return false };
    let any = match to {
        SocketAddr::V4(_) => SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
        SocketAddr::V6(_) => SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0)),
    };
    let Ok(udp) = UdpSocket::bind(any) else { return false };
    let probe = version_probe();
    let deadline = Instant::now() + wait;
    let mut buf = vec![0u8; 2048];
    while Instant::now() < deadline {
        let _ = udp.try_send_to(&probe, to);
        let resend = (Instant::now() + Duration::from_secs(1)).min(deadline);
        loop {
            let left = resend.saturating_duration_since(Instant::now());
            match crate::rt::timeout(left, udp.recv_from(&mut buf)).await {
                Ok(Ok((_, from))) if from == to => return true,
                Ok(Ok(_)) => {}
                Ok(Err(_)) => return false,
                Err(_) => break,
            }
        }
    }
    false
}

/// A long-header packet of a reserved version, as large as a QUIC client's first datagram (a
/// server answers only datagrams that large: §14.1).
fn version_probe() -> Vec<u8> {
    let mut p = Vec::with_capacity(1200);
    p.push(0xc0);
    p.extend_from_slice(&0x1a2a_3a4a_u32.to_be_bytes());
    for id in [rand::random::<[u8; 8]>(), rand::random::<[u8; 8]>()] {
        p.push(8);
        p.extend_from_slice(&id);
    }
    p.resize(1200, 0);
    p
}

/// An agent's choice of path to the server's QUIC listener (see the module docs).
pub struct Paths {
    server_url: String,
    /// The QUIC address as configured (`host:port`).
    quic: String,
    via: Transport,
    relay: Option<Relay>,
    /// When the dial in flight started.
    dialing: Option<Instant>,
    /// Failed dials in a row on the current path, and when the first of them started.
    misses: u32,
    missing_since: Option<Instant>,
    next_probe: Option<Instant>,
    probe: Option<futures::channel::oneshot::Receiver<bool>>,
    /// The path last reported connected (logged once per change).
    reported: Option<Transport>,
    /// Where the path last connected by is kept, for the next process.
    memory: Option<std::path::PathBuf>,
}

impl Paths {
    pub fn new(server_url: &str, quic: &str) -> Self {
        Self {
            server_url: server_url.to_owned(),
            quic: quic.to_owned(),
            via: Transport::Direct,
            relay: None,
            dialing: None,
            misses: 0,
            missing_since: None,
            next_probe: None,
            probe: None,
            reported: None,
            memory: None,
        }
    }

    /// [`Self::new`], starting with the path it last connected by, as kept in `file`: the tunnel
    /// first, if that is what it last took (UDP is checked again a while after connecting).
    pub fn remembering(server_url: &str, quic: &str, file: std::path::PathBuf) -> Self {
        let mut p = Self::new(server_url, quic);
        if std::fs::read_to_string(&file).is_ok_and(|s| s.trim() == "tunnel") {
            tracing::info!(quic = %quic, "this runner last reached the server through the HTTP tunnel; taking it first");
            p.via = Transport::Tunnel;
        }
        p.memory = Some(file);
        p
    }

    /// The path the next dial takes.
    pub fn via(&self) -> Transport {
        self.via
    }

    fn relay_alive(&self) -> bool {
        self.relay.as_ref().is_some_and(Relay::alive)
    }

    fn take(&mut self, via: Transport) {
        self.via = via;
        self.misses = 0;
        self.missing_since = None;
    }

    /// Where the next dial goes: the QUIC address, or the tunnel's relay (opened now when needed; a
    /// tunnel that cannot open sends the dial direct again).
    pub async fn target(&mut self, client: &Client, now: Instant) -> anyhow::Result<SocketAddr> {
        self.dialing = Some(now);
        if self.via == Transport::Tunnel {
            if !self.relay_alive() {
                self.relay = None;
                match Relay::open(client, &self.server_url).await {
                    Ok(r) => {
                        tracing::info!(relay = %r.addr(), "HTTP tunnel open");
                        self.relay = Some(r);
                    }
                    Err(e) => {
                        tracing::warn!(error = %format!("{e:#}"), quic = %self.quic, "cannot open the HTTP tunnel; dialing UDP again");
                        self.take(Transport::Direct);
                    }
                }
            }
            if let Some(r) = &self.relay {
                return Ok(r.addr());
            }
        }
        crate::rt::resolve(&self.quic).await.with_context(|| format!("resolve {}", self.quic))
    }

    /// The dial in flight failed. Returns whether to dial again at once (the path changed).
    pub fn dial_failed(&mut self, now: Instant) -> bool {
        let started = self.dialing.take().unwrap_or(now);
        self.misses = self.misses.saturating_add(1);
        let since = *self.missing_since.get_or_insert(started);
        match self.via {
            // Back to a tunnel still open (a switch back that did not work) at once.
            Transport::Direct if self.relay_alive() => {
                tracing::info!(quic = %self.quic, "UDP to the server still gets no answer; back to the HTTP tunnel");
                self.take(Transport::Tunnel);
                true
            }
            Transport::Direct if self.misses >= DIRECT_TRIES || now.saturating_duration_since(since) >= DIRECT_PATIENCE => {
                tracing::info!(quic = %self.quic, dials = self.misses, "UDP to the server gets no answer; trying the HTTP tunnel");
                self.take(Transport::Tunnel);
                true
            }
            Transport::Tunnel => {
                if !self.relay_alive() {
                    self.relay = None;
                }
                if self.misses >= DIRECT_TRIES {
                    tracing::info!(quic = %self.quic, "no answer through the HTTP tunnel either; dialing UDP again");
                    self.take(Transport::Direct);
                    return true;
                }
                false
            }
            Transport::Direct => false,
        }
    }

    /// The connection is up; `binding` is its exporter value (named to the server through a
    /// tunnel).
    pub fn connected(&mut self, binding: Option<[u8; 32]>, now: Instant) {
        self.dialing = None;
        self.misses = 0;
        self.missing_since = None;
        match self.via {
            Transport::Direct => {
                // Closes a tunnel kept open while switching back.
                self.relay = None;
                self.next_probe = None;
                self.probe = None;
                if self.reported != Some(Transport::Direct) {
                    tracing::info!(quic = %self.quic, "connected directly");
                }
            }
            Transport::Tunnel => {
                if let (Some(r), Some(b)) = (&self.relay, binding) {
                    r.bind(b);
                }
                self.next_probe = Some(now + PROBE_EVERY);
                if self.reported != Some(Transport::Tunnel) {
                    tracing::info!("connected through the HTTP tunnel (UDP {} unreachable)", self.quic);
                }
            }
        }
        if self.reported != Some(self.via)
            && let Some(file) = &self.memory
        {
            let _ = match self.via {
                Transport::Tunnel => std::fs::write(file, "tunnel\n"),
                Transport::Direct => std::fs::remove_file(file).or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) }),
            };
        }
        self.reported = Some(self.via);
    }

    /// The connection that was up ended.
    pub fn disconnected(&mut self) {
        self.dialing = None;
        self.probe = None;
        // A dead tunnel is opened again by the next dial.
        if !self.relay_alive() {
            self.relay = None;
        }
    }

    /// Periodically (the agent's watchdog). Returns whether to dial again now: direct dials have
    /// gone unanswered for [`DIRECT_PATIENCE`] (one still pending), or, tunnelled, UDP answers
    /// again (the caller drops the connection first; this one stays as a fallback until the
    /// direct one is up).
    pub fn tick(&mut self, connected: bool, now: Instant) -> bool {
        match self.via {
            Transport::Direct => {
                let overdue = self.missing_since.is_some_and(|t| now.saturating_duration_since(t) >= DIRECT_PATIENCE);
                if !connected && self.dialing.is_some() && self.misses > 0 && overdue {
                    tracing::info!(quic = %self.quic, after = ?DIRECT_PATIENCE, "UDP to the server gets no answer; trying the HTTP tunnel");
                    self.take(Transport::Tunnel);
                    return true;
                }
                false
            }
            Transport::Tunnel if connected => {
                if let Some(rx) = &mut self.probe {
                    match rx.try_recv() {
                        Ok(None) => return false,
                        Ok(Some(true)) => {
                            self.probe = None;
                            tracing::info!(quic = %self.quic, "UDP to the server answers again; switching to a direct connection");
                            self.take(Transport::Direct);
                            return true;
                        }
                        Ok(Some(false)) | Err(_) => {
                            self.probe = None;
                            self.next_probe = Some(now + PROBE_EVERY);
                        }
                    }
                }
                if self.next_probe.is_some_and(|t| now >= t) {
                    self.next_probe = None;
                    let (tx, rx) = futures::channel::oneshot::channel();
                    let quic = self.quic.clone();
                    crate::rt::spawn(async move {
                        let _ = tx.send(udp_answers(&quic, PROBE_WAIT).await);
                    });
                    self.probe = Some(rx);
                }
                false
            }
            Transport::Tunnel => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datagrams_and_bindings_frame_as_messages() {
        // One datagram is one binary message, up to the bound; empty and longer ones are dropped.
        let d = vec![7u8; MAX_DATAGRAM];
        let m = outbound(&d);
        assert!(matches!(&m, Some(Message::Binary(b)) if b.as_ref() == d.as_slice()));
        assert_eq!(m.as_ref().map(inbound), Some(Inbound::Datagram(&d)));
        assert!(outbound(&[]).is_none());
        assert!(outbound(&vec![1u8; MAX_DATAGRAM + 1]).is_none());
        assert_eq!(inbound(&ws::binary(vec![1u8; MAX_DATAGRAM + 1])), Inbound::Skip);
        assert_eq!(inbound(&ws::binary(Vec::new())), Inbound::Skip);

        // A binding round-trips through its text form; anything else in text is ignored.
        let b: [u8; 32] = std::array::from_fn(|i| u8::try_from(i).unwrap_or(0).wrapping_mul(7));
        assert_eq!(inbound(&bind_text(&b)), Inbound::Bind(b));
        assert_eq!(inbound(&ws::text("bind 00")), Inbound::Skip);
        assert_eq!(inbound(&ws::text(format!("bind {}", "zz".repeat(32)))), Inbound::Skip);
        assert_eq!(inbound(&ws::text("hello")), Inbound::Skip);

        // Control frames carry nothing to relay; a close ends the tunnel.
        assert_eq!(inbound(&Message::Ping(vec![1].into())), Inbound::Skip);
        assert_eq!(inbound(&Message::Pong(vec![1].into())), Inbound::Skip);
        assert_eq!(inbound(&Message::Close(None)), Inbound::Close);
    }

    #[test]
    fn the_listener_is_reached_where_it_listens() {
        let at = |s: &str| s.parse::<SocketAddr>().unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0)));
        assert_eq!(listener_for(at("0.0.0.0:7071")), at("127.0.0.1:7071"));
        assert_eq!(listener_for(at("[::]:7071")), at("[::1]:7071"));
        assert_eq!(listener_for(at("192.168.1.5:7071")), at("192.168.1.5:7071"));
        assert_eq!(listener_for(at("127.0.0.1:9")), at("127.0.0.1:9"));
        assert_eq!(listener_for(at("[fd00::5]:7071")), at("[fd00::5]:7071"));
        assert_eq!(local_end(at("127.0.0.1:7071")), at("127.0.0.1:0"));
        assert_eq!(local_end(at("[::1]:7071")), at("[::1]:0"));
    }

    #[test]
    fn tunnels_are_bounded_and_report_what_they_carry() -> anyhow::Result<()> {
        let reg = Arc::new(Registry::default());
        let now = Instant::now();
        let a: IpAddr = "10.0.0.1".parse()?;
        let b: IpAddr = "10.0.0.2".parse()?;
        // Per address: OPENS_PER_MINUTE a minute, then refused until the minute is over.
        let mut slots = Vec::new();
        for _ in 0..OPENS_PER_MINUTE {
            slots.push(reg.admit(Some(a), now).map_err(|r| anyhow::anyhow!("{r:?}"))?);
        }
        assert_eq!(reg.admit(Some(a), now).err(), Some(Refusal::TooOften));
        assert!(reg.admit(Some(b), now).is_ok());
        assert!(reg.admit(Some(a), now + MINUTE).is_ok());
        // Dropped slots are free again; at most MAX_TUNNELS at once.
        slots.clear();
        assert_eq!(reg.open(), 0);
        for i in 0..MAX_TUNNELS {
            slots.push(reg.admit(None, now + Duration::from_millis(u64::try_from(i).unwrap_or(0))).map_err(|r| anyhow::anyhow!("{r:?}"))?);
        }
        assert_eq!(reg.admit(None, now).err(), Some(Refusal::Full));
        slots.clear();

        // A connection is tunnelled while a live tunnel names its binding.
        let slot = reg.admit(None, now).map_err(|r| anyhow::anyhow!("{r:?}"))?;
        reg.connected(5, Some([1; 32]));
        reg.connected(6, Some([2; 32]));
        assert_eq!(reg.transport(5), Transport::Direct);
        slot.bind([1; 32]);
        assert_eq!(reg.transport(5), Transport::Tunnel);
        assert_eq!(reg.transport(6), Transport::Direct);
        // A new connection (new binding) of the same peer is direct until named.
        reg.connected(5, Some([3; 32]));
        assert_eq!(reg.transport(5), Transport::Direct);
        slot.bind([3; 32]);
        assert_eq!(reg.transport(5), Transport::Tunnel);
        drop(slot);
        assert_eq!(reg.transport(5), Transport::Direct);
        reg.closed(5);
        assert_eq!(reg.transport(5), Transport::Direct);
        Ok(())
    }

    #[test]
    fn the_path_last_connected_by_is_taken_first_next_time() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("transport");
        let now = Instant::now();
        // Nothing kept: UDP first, as ever.
        assert_eq!(Paths::remembering("http://server:7070", "server:7071", file.clone()).via(), Transport::Direct);
        // Connected through the tunnel: kept, and the next process starts with it.
        let mut p = Paths::remembering("http://server:7070", "server:7071", file.clone());
        p.via = Transport::Tunnel;
        p.connected(None, now);
        assert_eq!(std::fs::read_to_string(&file)?.trim(), "tunnel");
        let mut next = Paths::remembering("http://server:7070", "server:7071", file.clone());
        assert_eq!(next.via(), Transport::Tunnel);
        // Once UDP answers again and it connects directly, the next one starts direct again.
        next.take(Transport::Direct);
        next.connected(None, now);
        assert!(!file.exists());
        assert_eq!(Paths::remembering("http://server:7070", "server:7071", file).via(), Transport::Direct);
        Ok(())
    }

    #[test]
    fn paths_fall_back_to_the_tunnel_and_back() {
        let t0 = Instant::now();
        let s = |secs: u64| t0 + Duration::from_secs(secs);
        let mut p = Paths::new("http://server:7070", "server:7071");
        // One unanswered direct dial is not enough; the second is.
        p.dialing = Some(s(0));
        assert!(!p.dial_failed(s(15)));
        assert_eq!(p.via(), Transport::Direct);
        p.dialing = Some(s(15));
        assert!(p.dial_failed(s(30)));
        assert_eq!(p.via(), Transport::Tunnel);

        // Or the watchdog: the second direct dial still pending DIRECT_PATIENCE after the first began.
        let mut p = Paths::new("http://server:7070", "server:7071");
        p.dialing = Some(s(0));
        assert!(!p.dial_failed(s(15)));
        p.dialing = Some(s(16));
        assert!(!p.tick(false, s(19)));
        assert!(p.tick(false, s(20)));
        assert_eq!(p.via(), Transport::Tunnel);

        // Dials through the tunnel that fail twice send the agent back to UDP.
        p.dialing = Some(s(20));
        assert!(!p.dial_failed(s(35)));
        p.dialing = Some(s(36));
        assert!(p.dial_failed(s(50)));
        assert_eq!(p.via(), Transport::Direct);

        // Connected through the tunnel: the next UDP check is PROBE_EVERY away.
        p.take(Transport::Tunnel);
        p.connected(None, s(60));
        assert_eq!(p.reported, Some(Transport::Tunnel));
        assert_eq!(p.next_probe, Some(s(60) + PROBE_EVERY));
        // Not connected: no checks.
        assert!(!p.tick(false, s(60) + PROBE_EVERY));
        assert!(p.probe.is_none());
    }

    /// An HTTP server with only the tunnel route.
    struct TunnelOnly {
        registry: Arc<Registry>,
        listener: SocketAddr,
    }

    impl crate::http::server::Handler for TunnelOnly {
        async fn handle(self: Arc<Self>, req: Request) -> Response {
            serve(&self.registry, self.listener, req).unwrap_or_else(|(status, why)| crate::http::server::text(status, why))
        }
    }

    fn rt(e: hyper_rt::RtError) -> anyhow::Error {
        anyhow::anyhow!("{e}")
    }

    async fn eventually(what: &str, check: impl Fn() -> bool) -> anyhow::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !check() {
            anyhow::ensure!(Instant::now() < deadline, "timed out waiting until {what}");
            crate::rt::sleep(Duration::from_millis(20)).await;
        }
        Ok(())
    }

    /// A server's HTTP port serving only the tunnel route, relaying to `listener`.
    fn tunnel_server(listener: SocketAddr) -> anyhow::Result<(SocketAddr, Arc<Registry>)> {
        let tcp = std::net::TcpListener::bind("127.0.0.1:0")?;
        let http = tcp.local_addr()?;
        let tcp = hyper_rt::tcp::TcpListener::from_owned(hyper_rt::tcp::OwnedStream::from(tcp), 16).map_err(rt)?;
        let registry = Arc::new(Registry::default());
        let handler = Arc::new(TunnelOnly { registry: Arc::clone(&registry), listener });
        crate::rt::spawn(async move {
            while let Ok(stream) = tcp.accept().await {
                let (h, remote) = (Arc::clone(&handler), stream.peer_addr().ok());
                crate::rt::spawn(crate::http::server::serve_connection(crate::http::io::RtIo::new(stream), h, remote));
            }
        });
        Ok((http, registry))
    }

    #[test]
    fn datagrams_cross_a_tunnel_whole_both_ways() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            // The "QUIC listener": echoes every datagram to its sender.
            let echo = UdpSocket::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).map_err(rt)?;
            let listener = echo.local_addr().map_err(rt)?;
            crate::rt::spawn(async move {
                let mut buf = vec![0u8; 4096];
                while let Ok((n, from)) = echo.recv_from(&mut buf).await {
                    let _ = echo.try_send_to(buf.get(..n).unwrap_or_default(), from);
                }
            });
            let (http, registry) = tunnel_server(listener)?;

            // The agent's end, and its "QUIC endpoint".
            let relay = Relay::open(&Client::public()?, &format!("http://{http}/")).await?;
            let quic = UdpSocket::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).map_err(rt)?;
            let mut buf = vec![0u8; 4096];
            let mut round_trip = async |d: &[u8]| -> anyhow::Result<Option<Vec<u8>>> {
                quic.try_send_to(d, relay.addr()).map_err(rt)?;
                match crate::rt::timeout(Duration::from_millis(1500), quic.recv_from(&mut buf)).await {
                    Ok(r) => {
                        let (n, from) = r.map_err(rt)?;
                        anyhow::ensure!(from == relay.addr(), "answer from {from}");
                        Ok(buf.get(..n).map(<[u8]>::to_vec))
                    }
                    Err(_) => Ok(None),
                }
            };
            // Each datagram comes back whole, one message each way, up to the bound.
            for size in [1usize, 2, 125, 126, 1200, 1452, MAX_DATAGRAM] {
                let d: Vec<u8> = (0..size).map(|i| u8::try_from(i % 253).unwrap_or(0)).collect();
                assert_eq!(round_trip(&d).await?, Some(d), "{size} bytes");
            }
            // One too large is dropped, and the tunnel carries on.
            assert_eq!(round_trip(&vec![9u8; MAX_DATAGRAM + 1]).await?, None);
            assert_eq!(round_trip(b"still here").await?, Some(b"still here".to_vec()));
            assert_eq!(registry.open(), 1);

            // A binding named through the tunnel marks that connection tunnelled...
            registry.connected(9, Some([4; 32]));
            assert_eq!(registry.transport(9), Transport::Direct);
            relay.bind([4; 32]);
            eventually("the binding arrives", || registry.transport(9) == Transport::Tunnel).await?;
            // ...until the agent closes the tunnel, which frees its place on the server.
            drop(relay);
            eventually("the tunnel closes", || registry.open() == 0).await?;
            assert_eq!(registry.transport(9), Transport::Direct);
            Ok(())
        })
    }

    #[test]
    fn quic_with_mutual_tls_runs_through_a_tunnel() -> anyhow::Result<()> {
        use crate::net::{Kind, NetEvent, Out, SERVER_PEER};
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let pki = crate::pki::Pki::open(dir.path())?;
            let quic = std::net::UdpSocket::bind("127.0.0.1:0")?.local_addr()?;
            let spec = |bind, leaf, server| crate::net::Spec {
                bind,
                ca_der: pki.ca_der.clone(),
                leaf,
                server,
                dial_deadline: Duration::from_secs(10),
                revoked_keys: Vec::new(),
            };
            let (_server, server_ev) = crate::net::start(spec(quic, pki.server_leaf(&[])?, true))?;
            let (http, registry) = tunnel_server(quic)?;
            let relay = Relay::open(&Client::public()?, &format!("http://{http}")).await?;
            // The agent dials the relay on loopback; TLS still checks the server's certificate
            // under its fixed name, and the server the agent's.
            let (agent, agent_ev) = crate::net::start(spec("0.0.0.0:0".parse()?, pki.issue_local("bench-1")?, false))?;
            agent.command(Out::Connect { addr: relay.addr() }).await;

            let deadline = Instant::now() + Duration::from_secs(20);
            let next = async |rx: &async_channel::Receiver<NetEvent>| -> anyhow::Result<NetEvent> {
                crate::rt::timeout(deadline.saturating_duration_since(Instant::now()), rx.recv())
                    .await
                    .map_err(|_| anyhow::anyhow!("timed out"))?
                    .map_err(|_| anyhow::anyhow!("event stream closed"))
            };
            let (mut peer, mut log, mut hello) = (None, false, false);
            while !(log && hello) {
                futures::select_biased! {
                    ev = next(&server_ev).fuse() => match ev? {
                        NetEvent::Connected { peer: p, name, binding } => {
                            assert_eq!(name.as_deref(), Some("bench-1"));
                            registry.connected(p, binding);
                            peer = Some(p);
                        }
                        NetEvent::Frame { kind: Kind::Log, data, .. } => log = data.len() == 1 << 20,
                        NetEvent::Frame { data, .. } => hello = data == b"hello server",
                        other => anyhow::bail!("server: {other:?}"),
                    },
                    ev = next(&agent_ev).fuse() => match ev? {
                        NetEvent::Connected { binding: Some(b), .. } => {
                            relay.bind(b);
                            agent.send(SERVER_PEER, 2, Kind::Log, vec![7u8; 1 << 20]).await;
                            agent.send(SERVER_PEER, 0, Kind::Control, b"hello server".to_vec()).await;
                        }
                        other => anyhow::bail!("agent: {other:?}"),
                    },
                }
            }
            // The server tells this connection came through the tunnel.
            let peer = peer.ok_or_else(|| anyhow::anyhow!("server saw no connection"))?;
            eventually("the tunnel names the connection", || registry.transport(peer) == Transport::Tunnel).await?;
            Ok(())
        })
    }

    #[test]
    fn udp_probe_gets_an_answer_only_from_a_quic_listener() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let pki = crate::pki::Pki::open(dir.path())?;
            let addr = std::net::UdpSocket::bind("127.0.0.1:0")?.local_addr()?;
            let (_server, _events) = crate::net::start(crate::net::Spec {
                bind: addr,
                ca_der: pki.ca_der.clone(),
                leaf: pki.server_leaf(&[])?,
                server: true,
                dial_deadline: crate::net::DIAL_DEADLINE,
                revoked_keys: Vec::new(),
            })?;
            assert!(udp_answers(&addr.to_string(), Duration::from_secs(5)).await, "no Version Negotiation from {addr}");
            // Nothing listens there: no answer.
            let dead = std::net::UdpSocket::bind("127.0.0.1:0")?.local_addr()?;
            let t = Instant::now();
            assert!(!udp_answers(&dead.to_string(), Duration::from_millis(1500)).await);
            assert!(t.elapsed() >= Duration::from_millis(1400));
            Ok(())
        })
    }
}
