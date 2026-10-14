//! QUIC transport on hyper-raft's `hyper-transport` (mutual TLS, prioritised lanes) driven by
//! `hyper-tokio`.
//!
//! The endpoint is sans-io and single-owner, so each endpoint lives on its own OS thread with a
//! current-thread runtime and talks to the rest of kiln over bounded channels:
//!
//! ```text
//!   app ──Out──▶ [ net thread: Driver<Endpoint> ⇄ UDP ] ──NetEvent──▶ app
//! ```
//!
//! Lanes have no "writable" event, so frames refused with `LaneFull` wait in a bounded per-lane
//! outbox and are retried as the connection drains. A panic inside the loop is caught, the
//! endpoint rebuilt, and the loop resumed; peers simply reconnect.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::time::{Duration, Instant};

use hyper_tokio::{Driver, Io};
use hyper_transport::tls::{CertificateDer, Credentials, PrivateKeyDer, Tls};
use hyper_transport::{
    AdmissionLimits, Authenticated, Classes, Config, Directory, Endpoint, Event, Fixed, Limits,
    PeerId, RECEIVE_CHUNK, Refusal,
};
use tokio::sync::mpsc;

use crate::pki::Leaf;

/// The server is always peer 1 from an agent's point of view.
pub const SERVER_PEER: PeerId = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    Interactive,
    Bulk,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Control,
    Term,
    Log,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Server,
    Agent,
}

pub struct Kiln;

impl Classes for Kiln {
    type Class = Class;
    type Kind = Kind;
    type Role = Role;
    const RANKS: u8 = 2;

    fn rank(class: Class) -> u8 {
        match class {
            Class::Interactive => 0,
            Class::Bulk => 1,
        }
    }

    fn class_of(kind: Kind, _role: Role) -> Option<Class> {
        Some(match kind {
            Kind::Control | Kind::Term => Class::Interactive,
            Kind::Log => Class::Bulk,
        })
    }

    fn frame_bound(class: Class) -> u64 {
        match class {
            Class::Interactive => 1 << 20,
            Class::Bulk => 4 << 20,
        }
    }

    fn kind_code(kind: Kind) -> u16 {
        match kind {
            Kind::Control => 1,
            Kind::Term => 2,
            Kind::Log => 3,
        }
    }

    fn kind_of(code: u16) -> Option<Kind> {
        match code {
            1 => Some(Kind::Control),
            2 => Some(Kind::Term),
            3 => Some(Kind::Log),
            _ => None,
        }
    }
}

/// Server-side directory: any certificate the TLS layer accepted was signed by the kiln CA, so it
/// is an enrolled agent. The peer id is derived from the certificate, and its SAN is the agent
/// name the session is bound to.
#[derive(Default)]
pub struct ServerBook {
    names: HashMap<PeerId, String>,
    revoked: std::collections::HashSet<String>,
}

fn cert_name(der: &[u8]) -> Option<String> {
    crate::pki::cert_dns_name(der)
}

fn peer_of(der: &[u8]) -> PeerId {
    let h = crate::util::sha256_hex(der);
    let n = u64::from_str_radix(h.get(..16).unwrap_or("2"), 16).unwrap_or(2);
    n.max(2)
}

impl Directory for ServerBook {
    type Role = Role;

    fn identify(&mut self, a: &Authenticated<'_>) -> Option<(PeerId, Role)> {
        let der = a.certificate?;
        let name = cert_name(der)?;
        if self.revoked.contains(&name) {
            return None;
        }
        let peer = peer_of(der);
        self.names.insert(peer, name);
        Some((peer, Role::Agent))
    }

    fn server_name(&self, _peer: PeerId) -> Option<&str> {
        None
    }
}

pub struct AgentBook;

impl Directory for AgentBook {
    type Role = Role;

    fn identify(&mut self, a: &Authenticated<'_>) -> Option<(PeerId, Role)> {
        a.certificate.map(|_| (SERVER_PEER, Role::Server))
    }

    fn server_name(&self, peer: PeerId) -> Option<&str> {
        (peer == SERVER_PEER).then_some(crate::pki::SERVER_NAME)
    }
}

/// Commands into the net thread.
#[derive(Debug)]
pub enum Out {
    Frame { peer: PeerId, lane: u32, kind: Kind, data: Vec<u8> },
    Disconnect { peer: PeerId },
    Revoke { name: String },
    /// Agent side: dial the server.
    Connect { addr: SocketAddr },
}

/// Events out of the net thread.
#[derive(Debug)]
pub enum NetEvent {
    Connected { peer: PeerId, name: Option<String> },
    Closed { peer: PeerId, reason: String },
    Unreachable { peer: PeerId, reason: String },
    Frame { peer: PeerId, lane: u32, kind: Kind, data: Vec<u8> },
}

#[derive(Clone)]
pub struct NetHandle {
    tx: mpsc::Sender<Out>,
}

impl NetHandle {
    /// Queue a frame; waits when the net thread is saturated (back-pressure).
    pub async fn send(&self, peer: PeerId, lane: u32, kind: Kind, data: Vec<u8>) -> bool {
        self.tx.send(Out::Frame { peer, lane, kind, data }).await.is_ok()
    }

    /// Queue without waiting; returns false when full (callers drop or retry).
    pub fn try_send(&self, peer: PeerId, lane: u32, kind: Kind, data: Vec<u8>) -> bool {
        self.tx.try_send(Out::Frame { peer, lane, kind, data }).is_ok()
    }

    pub async fn command(&self, out: Out) -> bool {
        self.tx.send(out).await.is_ok()
    }
}

/// Per-peer, per-lane frames refused with `LaneFull`, bounded.
const OUTBOX_FRAMES: usize = 8192;

pub fn limits(server: bool) -> Limits {
    let window: u64 = 64 << 20;
    Limits {
        admission: AdmissionLimits {
            pending: if server { 512 } else { 8 },
            identities: if server { 4096 } else { 4 },
            connections: if server { 4096 } else { 4 },
            per_identity: 2,
        },
        streams_per_connection: 64,
        exchanges: if server { 4096 } else { 256 },
        lanes_per_peer: 8,
        lane_window: 1024,
        max_head: 64 << 10,
        window_ceiling: window,
        serve_period: Duration::from_secs(2),
        idle_timeout: Duration::from_secs(30),
        keep_alive: Some(Duration::from_secs(5)),
        max_responses: 64,
        receive_chunks: usize::try_from(window).unwrap_or(usize::MAX) / RECEIVE_CHUNK + 1,
    }
}

fn credentials(ca_der: &[u8], leaf: &Leaf) -> anyhow::Result<Credentials> {
    use hyper_quic::rustls::pki_types::pem::PemObject as _;
    let chain = CertificateDer::pem_slice_iter(leaf.cert_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("leaf pem: {e:?}"))?;
    let key = PrivateKeyDer::from_pem_slice(leaf.key_pem.as_bytes())
        .map_err(|e| anyhow::anyhow!("key pem: {e:?}"))?;
    Ok(Credentials { chain, key, roots: vec![CertificateDer::from(ca_der.to_vec())] })
}

pub struct Spec {
    pub bind: SocketAddr,
    pub ca_der: Vec<u8>,
    pub leaf: Leaf,
    pub server: bool,
}

/// Start a net thread. Returns the command handle and the event stream.
pub fn start(spec: Spec) -> anyhow::Result<(NetHandle, mpsc::Receiver<NetEvent>)> {
    // Validate the TLS material up front so misconfiguration fails loudly at startup.
    let _ = Tls::mutual(&credentials(&spec.ca_der, &spec.leaf)?, spec.server)
        .map_err(|r| anyhow::anyhow!("tls config: {r}"))?;
    let (tx, rx) = mpsc::channel::<Out>(16_384);
    let (etx, erx) = mpsc::channel::<NetEvent>(16_384);
    let name = if spec.server { "kiln-net-server" } else { "kiln-net-agent" };
    std::thread::Builder::new().name(name.into()).spawn(move || thread_main(spec, rx, etx))?;
    Ok((NetHandle { tx }, erx))
}

fn thread_main(spec: Spec, mut rx: mpsc::Receiver<Out>, etx: mpsc::Sender<NetEvent>) {
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(error = %e, "net runtime failed to start");
            return;
        }
    };
    let mut backoff = Duration::from_millis(200);
    let mut last_dial: Option<SocketAddr> = None;
    loop {
        let started = Instant::now();
        let r = std::panic::catch_unwind(AssertUnwindSafe(|| {
            rt.block_on(run_loop(&spec, &mut rx, &etx, &mut last_dial))
        }));
        match r {
            Ok(Ok(())) => return, // command channel closed: owner is gone
            Ok(Err(e)) => tracing::error!(error = %format!("{e:#}"), "net loop failed; rebuilding endpoint"),
            Err(p) => {
                crate::metrics::global().panics_caught.inc();
                tracing::error!(panic = %crate::sup::panic_message(p.as_ref()), "net loop panicked; rebuilding endpoint");
            }
        }
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_millis(200);
        }
        std::thread::sleep(crate::sup::jitter(backoff));
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
}

type ServerDriver = Driver<Kiln, Fixed, ServerBook>;
type AgentDriver = Driver<Kiln, Fixed, AgentBook>;

enum AnyDriver {
    Server(ServerDriver),
    Agent(AgentDriver),
}

macro_rules! with_driver {
    ($d:expr, $x:ident => $body:expr) => {
        match $d {
            AnyDriver::Server($x) => $body,
            AnyDriver::Agent($x) => $body,
        }
    };
}

fn build(spec: &Spec) -> anyhow::Result<AnyDriver> {
    let tls = Tls::mutual(&credentials(&spec.ca_der, &spec.leaf)?, spec.server)
        .map_err(|r| anyhow::anyhow!("tls: {r}"))?;
    let budget = Fixed::new(if spec.server { 1 << 30 } else { 256 << 20 }, 256);
    let io = Io { batch: 64 };
    let now = Instant::now();
    if spec.server {
        let cfg = Config { tls, role: Role::Server, limits: limits(true), congestion: Default::default() };
        let ep = Endpoint::<Kiln, _, _>::new(cfg, budget, ServerBook::default(), now)
            .map_err(|r| anyhow::anyhow!("endpoint: {r}"))?;
        let d = Driver::bind(ep, spec.bind, io).map_err(|e| anyhow::anyhow!("bind {}: {e:?}", spec.bind))?;
        Ok(AnyDriver::Server(d))
    } else {
        let cfg = Config { tls, role: Role::Agent, limits: limits(false), congestion: Default::default() };
        let ep = Endpoint::<Kiln, _, _>::new(cfg, budget, AgentBook, now)
            .map_err(|r| anyhow::anyhow!("endpoint: {r}"))?;
        let d = Driver::bind(ep, spec.bind, io).map_err(|e| anyhow::anyhow!("bind {}: {e:?}", spec.bind))?;
        Ok(AnyDriver::Agent(d))
    }
}

struct Outbox {
    queues: HashMap<(PeerId, u32), VecDeque<(Kind, Vec<u8>)>>,
    dropped: u64,
}

impl Outbox {
    fn push(&mut self, peer: PeerId, lane: u32, kind: Kind, data: Vec<u8>) {
        let q = self.queues.entry((peer, lane)).or_default();
        if q.len() >= OUTBOX_FRAMES {
            // Bulk lanes shed their oldest frame; a full control lane means the peer is stuck.
            q.pop_front();
            self.dropped += 1;
        }
        q.push_back((kind, data));
    }

    fn is_empty(&self) -> bool {
        self.queues.values().all(VecDeque::is_empty)
    }

    fn forget(&mut self, peer: PeerId) {
        self.queues.retain(|(p, _), _| *p != peer);
    }
}

fn flush_outbox<C, B, D>(ep: &mut Endpoint<C, B, D>, outbox: &mut Outbox) -> bool
where
    C: Classes<Kind = Kind>,
    B: hyper_transport::Budget<C::Class>,
    D: Directory<Role = C::Role>,
{
    let mut sent_any = false;
    let mut dead = Vec::new();
    for ((peer, lane), q) in &mut outbox.queues {
        while let Some((kind, data)) = q.front() {
            match ep.send_frame(*peer, *lane, *kind, data) {
                Ok(()) => {
                    q.pop_front();
                    sent_any = true;
                }
                Err(Refusal::LaneFull) => break,
                Err(Refusal::NotConnected | Refusal::Closed) => {
                    dead.push(*peer);
                    break;
                }
                Err(r) => {
                    tracing::warn!(peer, lane, refusal = %r, "frame refused; dropping");
                    q.pop_front();
                }
            }
        }
    }
    for p in dead {
        outbox.forget(p);
    }
    outbox.queues.retain(|_, q| !q.is_empty());
    sent_any
}

async fn run_loop(
    spec: &Spec,
    rx: &mut mpsc::Receiver<Out>,
    etx: &mpsc::Sender<NetEvent>,
    last_dial: &mut Option<SocketAddr>,
) -> anyhow::Result<()> {
    let mut driver = build(spec)?;
    if let AnyDriver::Server(d) = &driver {
        tracing::info!(addr = ?d.local_addr(), "QUIC listening (hyper-transport, mTLS)");
    }
    // After a rebuild, an agent re-dials where it was connected.
    if let (AnyDriver::Agent(d), Some(addr)) = (&mut driver, *last_dial)
        && let Err(r) = d.endpoint().connect(Instant::now(), SERVER_PEER, addr) {
            tracing::warn!(refusal = %r, "redial failed");
        }
    let mut outbox = Outbox { queues: HashMap::new(), dropped: 0 };
    let mut retry = tokio::time::interval(Duration::from_millis(2));
    retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut events: Vec<NetEvent> = Vec::with_capacity(64);

    loop {
        let pending = !outbox.is_empty();
        tokio::select! {
            biased;
            cmd = rx.recv() => {
                let Some(cmd) = cmd else { return Ok(()) };
                let mut cmds = vec![cmd];
                while cmds.len() < 512 {
                    match rx.try_recv() { Ok(c) => cmds.push(c), Err(_) => break }
                }
                for cmd in cmds {
                    match cmd {
                        Out::Frame { peer, lane, kind, data } => outbox.push(peer, lane, kind, data),
                        Out::Disconnect { peer } => {
                            outbox.forget(peer);
                            with_driver!(&mut driver, d => d.endpoint().disconnect(Instant::now(), peer));
                        }
                        Out::Revoke { name } => {
                            if let AnyDriver::Server(d) = &mut driver {
                                let book = d.endpoint().directory_mut();
                                book.revoked.insert(name.clone());
                                let peers: Vec<PeerId> = book.names.iter().filter(|(_, n)| **n == name).map(|(p, _)| *p).collect();
                                for p in peers { d.endpoint().disconnect(Instant::now(), p); }
                            }
                        }
                        Out::Connect { addr } => {
                            *last_dial = Some(addr);
                            if let AnyDriver::Agent(d) = &mut driver
                                && let Err(r) = d.endpoint().connect(Instant::now(), SERVER_PEER, addr) {
                                    events.push(NetEvent::Unreachable { peer: SERVER_PEER, reason: r.to_string() });
                                }
                        }
                    }
                }
                with_driver!(&mut driver, d => { flush_outbox(d.endpoint(), &mut outbox); d.flush(); });
            }
            ev = async { with_driver!(&mut driver, d => d.event().await) } => {
                let ev = ev.map_err(|e| anyhow::anyhow!("driver: {e:?}"))?;
                with_driver!(&mut driver, d => {
                    on_event(d.endpoint(), ev, &mut events, &mut outbox);
                    while let Some(ev) = d.endpoint().poll_event() {
                        on_event(d.endpoint(), ev, &mut events, &mut outbox);
                    }
                    if !outbox.is_empty() { flush_outbox(d.endpoint(), &mut outbox); d.flush(); }
                });
            }
            _ = retry.tick(), if pending => {
                with_driver!(&mut driver, d => { if flush_outbox(d.endpoint(), &mut outbox) { d.flush(); } });
            }
        }
        for ev in events.drain(..) {
            if etx.send(ev).await.is_err() {
                return Ok(());
            }
        }
    }
}

trait PeerNames {
    fn name_of(&self, peer: PeerId) -> Option<String>;
}

impl PeerNames for ServerBook {
    fn name_of(&self, peer: PeerId) -> Option<String> {
        self.names.get(&peer).cloned()
    }
}

impl PeerNames for AgentBook {
    fn name_of(&self, _peer: PeerId) -> Option<String> {
        Some(crate::pki::SERVER_NAME.to_owned())
    }
}

fn on_event<B, D>(
    ep: &mut Endpoint<Kiln, B, D>,
    ev: Event<Kiln>,
    out: &mut Vec<NetEvent>,
    outbox: &mut Outbox,
) where
    B: hyper_transport::Budget<Class>,
    D: Directory<Role = Role> + PeerNames,
{
    match ev {
        Event::Connected { peer, .. } => {
            out.push(NetEvent::Connected { peer, name: ep.directory().name_of(peer) });
        }
        Event::Frame { peer, lane, kind, frame } => {
            crate::metrics::global().quic_streams.inc();
            out.push(NetEvent::Frame { peer, lane, kind, data: frame.bytes().to_vec() });
            ep.release(frame);
        }
        Event::Closed { peer, refusal, .. } => {
            outbox.forget(peer);
            out.push(NetEvent::Closed {
                peer,
                reason: refusal.map_or_else(|| "closed".to_owned(), |r| r.to_string()),
            });
        }
        Event::Unreachable { peer, refusal } => {
            outbox.forget(peer);
            out.push(NetEvent::Unreachable {
                peer,
                reason: refusal.map_or_else(|| "unreachable".to_owned(), |r| r.to_string()),
            });
        }
        // kiln uses lanes only; refuse any exchange a peer tries to open.
        Event::Request { exchange, .. } => ep.refuse_exchange(exchange, Refusal::Kind),
        Event::Reply { exchange, .. } | Event::Refused { exchange, .. } => ep.end(exchange),
        Event::BodyReady { .. } | Event::Writable { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_and_server_exchange_frames() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pki = crate::pki::Pki::open(dir.path())?;
        let server_leaf = pki.server_leaf(&[])?;
        let probe = std::net::UdpSocket::bind("127.0.0.1:0")?;
        let addr = probe.local_addr()?;
        drop(probe);
        let (srv, mut srv_ev) =
            start(Spec { bind: addr, ca_der: pki.ca_der.clone(), leaf: server_leaf, server: true })?;
        let agent_leaf = pki.issue_local("bench-1")?;
        let (agent, mut agent_ev) = start(Spec {
            bind: "127.0.0.1:0".parse()?,
            ca_der: pki.ca_der.clone(),
            leaf: agent_leaf,
            server: false,
        })?;
        agent.command(Out::Connect { addr }).await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut agent_peer = None;
        loop {
            tokio::select! {
                Some(ev) = srv_ev.recv() => match ev {
                    NetEvent::Connected { peer, name } => {
                        assert_eq!(name.as_deref(), Some("bench-1"));
                        agent_peer = Some(peer);
                    }
                    NetEvent::Frame { kind: Kind::Log, data, .. } => assert_eq!(data.len(), 1 << 20),
                    NetEvent::Frame { peer, data, .. } => {
                        assert_eq!(data, b"hello server");
                        srv.send(peer, 0, Kind::Control, b"hello agent".to_vec()).await;
                    }
                    _ => {}
                },
                Some(ev) = agent_ev.recv() => match ev {
                    NetEvent::Connected { .. } => {
                        // Big log frame through the bulk lane, then a control frame.
                        agent.send(SERVER_PEER, 2, Kind::Log, vec![7u8; 1 << 20]).await;
                        agent.send(SERVER_PEER, 0, Kind::Control, b"hello server".to_vec()).await;
                    }
                    NetEvent::Frame { data, .. } => {
                        assert_eq!(data, b"hello agent");
                        assert!(agent_peer.is_some());
                        return Ok(());
                    }
                    _ => {}
                },
                () = tokio::time::sleep_until(deadline) => anyhow::bail!("timed out"),
            }
        }
    }
}
