//! QUIC transport on hyper-raft's `hyper-transport` (mutual TLS, prioritised lanes) driven by
//! `hyper-rt-transport` on hyper-rt.
//!
//! The endpoint is sans-io and single-owner, so each endpoint lives on its own OS thread running a
//! one-shard hyper-rt `LocalRuntime` (packet handling never waits behind the HTTP shards) and
//! talks to the rest of kiln over bounded channels:
//!
//! ```text
//!   app ──Out──▶ [ net thread: Driver<Endpoint> ⇄ UDP ] ──NetEvent──▶ app
//! ```
//!
//! Lanes have no "writable" event, so frames refused with `LaneFull` wait in a bounded per-lane
//! outbox and are retried as the connection drains. A failure or panic inside the loop ends that
//! incarnation; the thread rebuilds the endpoint on a fresh runtime and an agent re-dials. The
//! owner can force the same rebuild with [`Out::Rebuild`] (the agent's dial watchdog does).
//!
//! Every frame the loop lets go before the wire is counted, by lane and reason
//! (`crate::metrics::NetMetrics`, docs/wire.md "Loss below the channel"). A control frame is never
//! shed: a control lane whose queue is full means its peer is stuck, and the connection is closed
//! instead, so the channel's resume repairs what it carried. Other lanes shed their oldest frame.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use futures::FutureExt as _;
use hyper_rt::runtime::LocalRuntime;
use hyper_rt_transport::{Driver, Io};
use hyper_transport::tls::{CertificateDer, Credentials, PrivateKeyDer, Tls};
use hyper_transport::{
    AdmissionLimits, Authenticated, Classes, Config, Directory, Endpoint, Event, Fixed, Limits,
    PeerId, RECEIVE_CHUNK, Refusal,
};

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
        // A machine named as the server (a certificate from before machines couldn't be) is none.
        if self.revoked.contains(&name) || crate::pki::reserved(&name) {
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

/// The TLS exporter label (RFC 8446 §7.5) of a connection's binding ([`NetEvent::Connected`]).
pub const BINDING_LABEL: &[u8] = b"EXPORTER-kiln-transport-binding";

/// Commands into the net thread.
#[derive(Debug)]
pub enum Out {
    Frame { peer: PeerId, lane: u32, kind: Kind, data: Vec<u8> },
    Disconnect { peer: PeerId },
    Revoke { name: String },
    /// Accept that agent name again (an admin approved a new machine under it).
    Unrevoke { name: String },
    /// Agent side: dial the server.
    Connect { addr: SocketAddr },
    /// Drop the endpoint and build a new one (fresh socket and runtime); an agent re-dials.
    Rebuild,
    /// A test build's fault (`crate::runner::chaos`, `shed=`): frames dropped as [`Shed`] says,
    /// from now on.
    Shed(Shed),
}

/// About one frame in `every` of `lane` (of those whose first byte, a message's variant, is
/// `variant`, when it says) dropped before the wire, counted as `chaos`: a frame lost below the
/// channel. Which ones is a seeded draw, not every `every`th: a fixed period could fall on the
/// same item of each resend, as no real loss does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shed {
    pub lane: u32,
    pub variant: Option<u8>,
    pub every: u64,
}

/// Events out of the net thread.
#[derive(Debug)]
pub enum NetEvent {
    /// `binding`: the connection's TLS exporter value under [`BINDING_LABEL`], the same at both
    /// ends of one connection and unknowable to anyone else (see `crate::tunnel`).
    Connected { peer: PeerId, name: Option<String>, binding: Option<[u8; 32]> },
    Closed { peer: PeerId, reason: String },
    Unreachable { peer: PeerId, reason: String },
    Frame { peer: PeerId, lane: u32, kind: Kind, data: Vec<u8> },
}

#[derive(Clone)]
pub struct NetHandle {
    tx: async_channel::Sender<Out>,
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
        // Eight lanes kiln has always had, and `proto::LANE_STREAM` and `proto::LANE_PORTS` past
        // them (a peer opens one only when the other side announced it takes it).
        lanes_per_peer: 16,
        lane_window: 1024,
        max_head: 64 << 10,
        window_ceiling: window,
        serve_period: Duration::from_secs(2),
        idle_timeout: Duration::from_secs(10),
        keep_alive: Some(Duration::from_secs(2)),
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
    /// Agent side: a dial with no answer in this long is abandoned and reported `Unreachable`.
    pub dial_deadline: Duration,
}

/// How long an agent's dial may go unanswered (a server that is down, or not routable yet at
/// boot, drops the handshake silently; QUIC alone would keep the dial pending).
pub const DIAL_DEADLINE: Duration = Duration::from_secs(15);

/// Start a net thread. Returns the command handle and the event stream.
pub fn start(spec: Spec) -> anyhow::Result<(NetHandle, async_channel::Receiver<NetEvent>)> {
    // Validate the TLS material up front so misconfiguration fails loudly at startup.
    let _ = Tls::mutual(&credentials(&spec.ca_der, &spec.leaf)?, spec.server)
        .map_err(|r| anyhow::anyhow!("tls config: {r}"))?;
    let (tx, rx) = async_channel::bounded::<Out>(16_384);
    let (etx, erx) = async_channel::bounded::<NetEvent>(16_384);
    let name = if spec.server { "kiln-net-server" } else { "kiln-net-agent" };
    std::thread::Builder::new().name(name.into()).spawn(move || thread_main(spec, rx, etx))?;
    Ok((NetHandle { tx }, erx))
}

/// How one incarnation of the loop ended.
enum Exit {
    /// The command channel closed: the owner is gone.
    Closed,
    /// Start over on a fresh endpoint; `redial`: an agent dials its last address at once.
    Rebuild { redial: bool },
}

fn thread_main(spec: Spec, rx: async_channel::Receiver<Out>, etx: async_channel::Sender<NetEvent>) {
    let spec = Arc::new(spec);
    let last_dial: Arc<Mutex<Option<SocketAddr>>> = Arc::new(Mutex::new(None));
    // One shard: the loop, plus helpers for any wait polled under a foreign waker.
    let config = crate::rt::runtime_config(1, 1024);
    let mut backoff = Duration::from_millis(200);
    loop {
        let started = Instant::now();
        // A fresh runtime per incarnation: whatever a failed loop left behind goes with it.
        let outcome = match LocalRuntime::new(&config) {
            Ok(mut local) => local
                .block_on(run_loop(Arc::clone(&spec), rx.clone(), etx.clone(), Arc::clone(&last_dial)))
                .map_err(|e| format!("{e}")),
            Err(e) => Err(format!("cannot start the net runtime: {e}")),
        };
        match outcome {
            Ok(Ok(Exit::Closed)) => return,
            Ok(Ok(Exit::Rebuild { redial })) => {
                if !redial {
                    *last_dial.lock().unwrap_or_else(PoisonError::into_inner) = None;
                }
                tracing::info!(redial, "rebuilding the QUIC endpoint");
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            Ok(Err(e)) => tracing::error!(error = %format!("{e:#}"), "net loop failed; rebuilding endpoint"),
            Err(e) => {
                // RootUnwound: the loop panicked (contained by hyper-rt).
                crate::metrics::global().panics_caught.inc();
                tracing::error!(error = %e, "net loop panicked; rebuilding endpoint");
            }
        }
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_millis(200);
        }
        std::thread::sleep(crate::sup::jitter(backoff));
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
}

/// An agent's dial to the server.
#[derive(Clone, Copy)]
enum Dial {
    Idle,
    Dialing { addr: SocketAddr, since: Instant },
    Connected,
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

/// Builds the endpoint and binds its socket; must run on the net thread's shard.
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
        let d = Driver::bind(ep, spec.bind, io).map_err(|e| anyhow::anyhow!("bind {}: {e}", spec.bind))?;
        Ok(AnyDriver::Server(d))
    } else {
        let cfg = Config { tls, role: Role::Agent, limits: limits(false), congestion: Default::default() };
        let ep = Endpoint::<Kiln, _, _>::new(cfg, budget, AgentBook, now)
            .map_err(|r| anyhow::anyhow!("endpoint: {r}"))?;
        let d = Driver::bind(ep, spec.bind, io).map_err(|e| anyhow::anyhow!("bind {}: {e}", spec.bind))?;
        Ok(AnyDriver::Agent(d))
    }
}

/// "Now" for endpoint calls: the driver's clock (anchored to the shard clock).
macro_rules! clock {
    ($d:expr) => {
        $d.clock().unwrap_or_else(|_| Instant::now())
    };
}

#[derive(Default)]
struct Outbox {
    queues: HashMap<(PeerId, u32), VecDeque<(Kind, Vec<u8>)>>,
    /// A test build's fault, and its draw's state.
    shed: Option<(Shed, u64)>,
}

/// What became of a frame handed to the outbox.
#[derive(Debug, PartialEq, Eq)]
enum Push {
    Queued,
    /// Its lane is a control lane whose queue is full: its peer is stuck. Not queued: the caller
    /// closes the connection.
    Stuck,
}

impl Outbox {
    fn push(&mut self, peer: PeerId, lane: u32, kind: Kind, data: Vec<u8>) -> Push {
        if let Some((rule, draw)) = self.shed.as_mut()
            && rule.lane == lane
            && rule.variant.is_none_or(|v| data.first() == Some(&v))
        {
            // xorshift64*: the same frames each run, at no fixed period.
            *draw ^= *draw << 13;
            *draw ^= *draw >> 7;
            *draw ^= *draw << 17;
            if draw.wrapping_mul(0x2545_f491_4f6c_dd1d).checked_rem(rule.every.max(1)) == Some(0) {
                crate::metrics::net().dropped(peer, lane, "chaos", 1);
                return Push::Queued;
            }
        }
        let q = self.queues.entry((peer, lane)).or_default();
        if q.len() >= OUTBOX_FRAMES {
            if kind == Kind::Control {
                return Push::Stuck;
            }
            q.pop_front();
            crate::metrics::net().dropped(peer, lane, "shed", 1);
        }
        q.push_back((kind, data));
        Push::Queued
    }

    fn is_empty(&self) -> bool {
        self.queues.values().all(VecDeque::is_empty)
    }

    /// Lets go of what's queued for `peer`, counted as dropped for `reason`.
    fn forget(&mut self, peer: PeerId, reason: &'static str) {
        self.queues.retain(|(p, lane), q| {
            if *p != peer {
                return true;
            }
            crate::metrics::net().dropped(*p, *lane, reason, u64::try_from(q.len()).unwrap_or(u64::MAX));
            false
        });
    }
}

/// What's still queued when the loop ends (a rebuild, or the end) goes with it, counted.
impl Drop for Outbox {
    fn drop(&mut self) {
        for ((peer, lane), q) in &self.queues {
            crate::metrics::net().dropped(*peer, *lane, "ended", u64::try_from(q.len()).unwrap_or(u64::MAX));
        }
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
                    crate::metrics::net().dropped(*peer, *lane, "refused", 1);
                }
            }
        }
    }
    for p in dead {
        outbox.forget(p, "closed");
    }
    outbox.queues.retain(|_, q| !q.is_empty());
    sent_any
}

async fn run_loop(
    spec: Arc<Spec>,
    rx: async_channel::Receiver<Out>,
    etx: async_channel::Sender<NetEvent>,
    last_dial: Arc<Mutex<Option<SocketAddr>>>,
) -> anyhow::Result<Exit> {
    let mut driver = build(&spec)?;
    if let AnyDriver::Server(d) = &driver {
        tracing::info!(addr = ?d.local_addr(), "QUIC listening (hyper-transport on hyper-rt, mTLS)");
    }
    // An agent's dial to the server: hyper-transport keeps at most one per peer and ignores a
    // second `connect` while one is pending, so the loop tracks it to replace or abandon it.
    let mut dial = Dial::Idle;
    // After a rebuild, an agent re-dials where it was connected.
    let redial = *last_dial.lock().unwrap_or_else(PoisonError::into_inner);
    if let (AnyDriver::Agent(d), Some(addr)) = (&mut driver, redial) {
        let now = clock!(d);
        match d.endpoint().connect(now, SERVER_PEER, addr) {
            Ok(()) => dial = Dial::Dialing { addr, since: Instant::now() },
            Err(r) => tracing::warn!(refusal = %r, "redial failed"),
        }
        if let Err(e) = d.flush() {
            tracing::debug!(error = %e, "flush after redial");
        }
    }
    let mut outbox = Outbox::default();
    let mut events: Vec<NetEvent> = Vec::with_capacity(64);

    enum Ev {
        Cmd(Option<Out>),
        Driver(Result<Event<Kiln>, hyper_rt::RtError>),
        Retry,
        DialDeadline,
    }

    loop {
        let pending = !outbox.is_empty();
        let ev = {
            let cmd = rx.recv().fuse();
            // `event()` is cancel-safe: dropping it when another branch wins loses nothing.
            let drv = async { with_driver!(&mut driver, d => d.event().await) }.fuse();
            let retry = async {
                if pending {
                    crate::rt::sleep(Duration::from_millis(2)).await;
                } else {
                    futures::future::pending::<()>().await;
                }
            }
            .fuse();
            let deadline = match dial {
                Dial::Dialing { since, .. } => Some(since + spec.dial_deadline),
                _ => None,
            };
            let dial_timer = async {
                match deadline {
                    Some(at) => crate::rt::sleep_until(at).await,
                    None => futures::future::pending::<()>().await,
                }
            }
            .fuse();
            futures::pin_mut!(cmd, drv, retry, dial_timer);
            futures::select_biased! {
                c = cmd => Ev::Cmd(c.ok()),
                e = drv => Ev::Driver(e),
                () = dial_timer => Ev::DialDeadline,
                () = retry => Ev::Retry,
            }
        };
        match ev {
            Ev::Cmd(None) => return Ok(Exit::Closed),
            Ev::Cmd(Some(first)) => {
                let mut cmds = vec![first];
                while cmds.len() < 512 {
                    match rx.try_recv() {
                        Ok(c) => cmds.push(c),
                        Err(_) => break,
                    }
                }
                for cmd in cmds {
                    match cmd {
                        Out::Frame { peer, lane, kind, data } => {
                            if outbox.push(peer, lane, kind, data) == Push::Stuck {
                                // Its control lane's queue is full: rather than lose control
                                // frames, the connection closes, and the channel's resume repairs
                                // what it carried.
                                tracing::warn!(peer, lane, frames = OUTBOX_FRAMES, "a control lane's queue is full, its peer stuck: its connection closes");
                                crate::metrics::net().dropped(peer, lane, "stuck", 1);
                                outbox.forget(peer, "stuck");
                                with_driver!(&mut driver, d => {
                                    let now = clock!(d);
                                    d.endpoint().disconnect(now, peer);
                                });
                            }
                        }
                        Out::Shed(rule) => {
                            tracing::warn!(?rule, "chaos: frames are shed below the channel");
                            outbox.shed = Some((rule, 0x9e37_79b9_7f4a_7c15));
                        }
                        Out::Disconnect { peer } => {
                            outbox.forget(peer, "closed");
                            with_driver!(&mut driver, d => {
                                let now = clock!(d);
                                d.endpoint().disconnect(now, peer);
                            });
                        }
                        Out::Revoke { name } => {
                            if let AnyDriver::Server(d) = &mut driver {
                                let now = clock!(d);
                                let book = d.endpoint().directory_mut();
                                book.revoked.insert(name.clone());
                                let peers: Vec<PeerId> =
                                    book.names.iter().filter(|(_, n)| **n == name).map(|(p, _)| *p).collect();
                                for p in peers {
                                    d.endpoint().disconnect(now, p);
                                }
                            }
                        }
                        Out::Unrevoke { name } => {
                            if let AnyDriver::Server(d) = &mut driver {
                                d.endpoint().directory_mut().revoked.remove(&name);
                            }
                        }
                        Out::Connect { addr } => {
                            *last_dial.lock().unwrap_or_else(PoisonError::into_inner) = Some(addr);
                            if let AnyDriver::Agent(d) = &mut driver {
                                match dial {
                                    // Already dialing there: let that dial finish.
                                    Dial::Dialing { addr: pending, .. } if pending == addr => {}
                                    // Dialing elsewhere, or the owner believes the connection is
                                    // gone: a fresh endpoint dials the new address at once.
                                    Dial::Dialing { .. } | Dial::Connected => return Ok(Exit::Rebuild { redial: true }),
                                    Dial::Idle => {
                                        let now = clock!(d);
                                        match d.endpoint().connect(now, SERVER_PEER, addr) {
                                            Ok(()) => dial = Dial::Dialing { addr, since: Instant::now() },
                                            Err(r) => events.push(NetEvent::Unreachable { peer: SERVER_PEER, reason: r.to_string() }),
                                        }
                                    }
                                }
                            }
                        }
                        Out::Rebuild => return Ok(Exit::Rebuild { redial: true }),
                    }
                }
                with_driver!(&mut driver, d => {
                    flush_outbox(d.endpoint(), &mut outbox);
                    if let Err(e) = d.flush() {
                        tracing::debug!(error = %e, "flush");
                    }
                });
            }
            Ev::Driver(ev) => {
                let ev = ev.map_err(|e| anyhow::anyhow!("driver: {e}"))?;
                with_driver!(&mut driver, d => {
                    on_event(d.endpoint(), ev, &mut events, &mut outbox);
                    while let Some(ev) = d.endpoint().poll_event() {
                        on_event(d.endpoint(), ev, &mut events, &mut outbox);
                    }
                    if !outbox.is_empty() {
                        flush_outbox(d.endpoint(), &mut outbox);
                        if let Err(e) = d.flush() {
                            tracing::debug!(error = %e, "flush");
                        }
                    }
                });
            }
            Ev::DialDeadline => {
                if let Dial::Dialing { addr, .. } = dial {
                    tracing::warn!(%addr, after = ?spec.dial_deadline, "no answer to the dial; abandoning it");
                    // Report first, then start over clean: the owner re-dials with its backoff.
                    let _ = etx
                        .send(NetEvent::Unreachable {
                            peer: SERVER_PEER,
                            reason: format!("no answer from {addr} in {}s", spec.dial_deadline.as_secs()),
                        })
                        .await;
                    return Ok(Exit::Rebuild { redial: false });
                }
            }
            Ev::Retry => {
                with_driver!(&mut driver, d => {
                    if flush_outbox(d.endpoint(), &mut outbox)
                        && let Err(e) = d.flush()
                    {
                        tracing::debug!(error = %e, "flush");
                    }
                });
            }
        }
        for ev in events.drain(..) {
            if !spec.server {
                match &ev {
                    NetEvent::Connected { peer, .. } if *peer == SERVER_PEER => dial = Dial::Connected,
                    NetEvent::Closed { peer, .. } | NetEvent::Unreachable { peer, .. } if *peer == SERVER_PEER => {
                        dial = Dial::Idle;
                    }
                    _ => {}
                }
            }
            if etx.send(ev).await.is_err() {
                return Ok(Exit::Closed);
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
            let mut binding = [0u8; 32];
            let binding = ep.export_keying_material(peer, BINDING_LABEL, b"", &mut binding).ok().map(|_| binding);
            out.push(NetEvent::Connected { peer, name: ep.directory().name_of(peer), binding });
        }
        Event::Frame { peer, lane, kind, frame } => {
            crate::metrics::global().quic_streams.inc();
            out.push(NetEvent::Frame { peer, lane, kind, data: frame.bytes().to_vec() });
            ep.release(frame);
        }
        Event::Closed { peer, refusal, .. } => {
            outbox.forget(peer, "closed");
            out.push(NetEvent::Closed {
                peer,
                reason: refusal.map_or_else(|| "closed".to_owned(), |r| r.to_string()),
            });
        }
        Event::Unreachable { peer, refusal } => {
            outbox.forget(peer, "closed");
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

    async fn next_event(rx: &async_channel::Receiver<NetEvent>, deadline: Instant) -> anyhow::Result<NetEvent> {
        crate::rt::timeout(deadline.saturating_duration_since(Instant::now()), rx.recv())
            .await
            .map_err(|_| anyhow::anyhow!("timed out"))?
            .map_err(|_| anyhow::anyhow!("event stream closed"))
    }

    /// What the outbox lets go is counted by lane and reason, and a control frame is never shed:
    /// a control lane whose queue is full is its peer stuck, for the caller to close.
    #[test]
    fn the_outbox_counts_what_it_lets_go_and_never_sheds_a_control_frame() {
        let m = crate::metrics::net();
        // A peer and lanes of their own ("other" to the metric), so no other test counts with them.
        let (peer, bulk, control): (PeerId, u32, u32) = (0xdead_0008, 13, 12);
        let count = |reason: &str| m.frames_dropped.get(&["other", reason]);
        let before: Vec<u64> = ["shed", "closed", "chaos", "ended"].iter().map(|r| count(r)).collect();
        let grew = |i: usize, reason: &str| count(reason) - before.get(i).copied().unwrap_or(0);
        let mut o = Outbox::default();
        for _ in 0..=OUTBOX_FRAMES {
            assert_eq!(o.push(peer, bulk, Kind::Log, vec![1]), Push::Queued);
        }
        assert_eq!(grew(0, "shed"), 1, "a full lane that may lose frames sheds its oldest, counted");
        for _ in 0..OUTBOX_FRAMES {
            assert_eq!(o.push(peer, control, Kind::Control, vec![2]), Push::Queued);
        }
        assert_eq!(o.push(peer, control, Kind::Control, vec![3]), Push::Stuck, "a full control lane sheds nothing");
        assert_eq!(grew(0, "shed"), 1);
        o.forget(peer, "closed");
        assert_eq!(grew(1, "closed"), 2 * OUTBOX_FRAMES as u64, "what a closed connection had queued, counted");
        // A test build's fault: every one of the bulk lane's frames whose first byte is 9.
        o.shed = Some((Shed { lane: bulk, variant: Some(9), every: 1 }, 0x9e37_79b9_7f4a_7c15));
        for b in [9, 8, 9, 9, 9, 9] {
            assert_eq!(o.push(peer, bulk, Kind::Log, vec![b]), Push::Queued);
        }
        assert_eq!(grew(2, "chaos"), 5);
        // One in three: some, not all, of 300.
        o.shed = Some((Shed { lane: bulk, variant: None, every: 3 }, 0x9e37_79b9_7f4a_7c15));
        for _ in 0..300 {
            o.push(peer, bulk, Kind::Log, vec![7]);
        }
        let some = grew(2, "chaos") - 5;
        assert!((50..150).contains(&some), "about one in three dropped: {some} of 300");
        drop(o);
        assert_eq!(grew(3, "ended"), 1 + 300 - some, "what the loop still held when it ended, counted");
        let toward: u64 = m.toward(peer).iter().map(|d| d.frames).sum();
        assert_eq!(toward, 1 + 2 * OUTBOX_FRAMES as u64 + 5 + 300 + 1, "and counted by peer, for its runner's page");
    }

    /// Plan step 8 of "Nothing kiln sends between its processes is fire-and-forget": frames lost
    /// below the channel inside a live connection, kiln's real QUIC layer between a runner and a
    /// server. The runner drops about one in 5 of its channel frames and the server one in 4 of its,
    /// while a stream of updates goes up and a stream of commands down. The channel makes up for
    /// each (a receiver's `Want`, a sender's patience): every item is taken once and answered, and
    /// each side counted what it dropped.
    #[test]
    fn what_is_lost_below_the_channel_is_sent_again_and_taken_once() -> anyhow::Result<()> {
        use crate::proto::{AgentMsg, LANE_CONTROL, LANE_TERM, ServerMsg};
        use crate::wire::channel::{Arrival, Event as Said, Frame, Outcome, Receiver, Sender};
        const N: u64 = 60;
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let pki = crate::pki::Pki::open(dir.path())?;
            let probe = std::net::UdpSocket::bind("127.0.0.1:0")?;
            let addr = probe.local_addr()?;
            drop(probe);
            let (srv, srv_ev) = start(Spec { bind: addr, ca_der: pki.ca_der.clone(), leaf: pki.server_leaf(&[])?, server: true, dial_deadline: DIAL_DEADLINE })?;
            let (agent, agent_ev) = start(Spec { bind: "127.0.0.1:0".parse()?, ca_der: pki.ca_der.clone(), leaf: pki.issue_local("shed-1")?, server: false, dial_deadline: DIAL_DEADLINE })?;
            let chaos = |lane: u32| crate::metrics::net().frames_dropped.get(&[crate::proto::lane_name(lane), "chaos"]);
            let (term0, control0) = (chaos(LANE_TERM), chaos(LANE_CONTROL));
            agent.command(Out::Shed(Shed { lane: LANE_TERM, variant: Some(46), every: 5 })).await;
            srv.command(Out::Shed(Shed { lane: LANE_CONTROL, variant: Some(54), every: 4 })).await;
            agent.command(Out::Connect { addr }).await;
            let patience = Duration::from_millis(300);
            // Updates up (the runner's sender, the server's receiver); commands down.
            let (mut up, mut up_in) = (Sender::new(1 << 20, patience), Receiver::new());
            let (mut down, mut down_in) = (Sender::new(1 << 20, patience), Receiver::new());
            for n in 0..N {
                up.push("s:x", n.to_be_bytes().to_vec()).map_err(|_| anyhow::anyhow!("full"))?;
                down.push("cmd", n.to_be_bytes().to_vec()).map_err(|_| anyhow::anyhow!("full"))?;
            }
            let index = |body: &[u8]| body.try_into().ok().map(u64::from_be_bytes).and_then(|n| usize::try_from(n).ok());
            let (mut took_up, mut took_down) = (vec![0u32; N as usize], vec![0u32; N as usize]);
            let (mut answered_up, mut answered_down) = (0u64, 0u64);
            let mut peer = None;
            let mut reconnects = 0;
            let deadline = Instant::now() + Duration::from_secs(60);
            while answered_up < N || answered_down < N {
                anyhow::ensure!(Instant::now() < deadline, "answered {answered_up} of {N} updates and {answered_down} of {N} commands, over {reconnects} reconnects");
                if let Some(p) = peer {
                    for f in down.poll(Instant::now()) {
                        srv.send(p, LANE_CONTROL, Kind::Control, crate::proto::encode(&ServerMsg::Wire(f))?).await;
                    }
                }
                for f in up.poll(Instant::now()) {
                    agent.send(SERVER_PEER, LANE_TERM, Kind::Term, crate::proto::encode(&AgentMsg::Wire(f))?).await;
                }
                // Every event that came: the first waited for (or a tick), then all the others.
                let mut evs: Vec<(bool, NetEvent)> = Vec::new();
                evs.extend(futures::select_biased! {
                    ev = srv_ev.recv().fuse() => ev.ok().map(|e| (true, e)),
                    ev = agent_ev.recv().fuse() => ev.ok().map(|e| (false, e)),
                    () = crate::rt::sleep(Duration::from_millis(50)).fuse() => None,
                });
                evs.extend(std::iter::from_fn(|| srv_ev.try_recv().ok()).map(|e| (true, e)));
                evs.extend(std::iter::from_fn(|| agent_ev.try_recv().ok()).map(|e| (false, e)));
                // Each side's frames to the other, and what it takes of the other's.
                let mut answers: Vec<(bool, Frame)> = Vec::new();
                for ev in evs {
                    match ev {
                        (true, NetEvent::Connected { peer: p, .. }) => {
                            peer = Some(p);
                            answers.push((true, up_in.have()));
                        }
                        (false, NetEvent::Connected { .. }) => answers.push((false, down_in.have())),
                        // A connection closed (a stuck lane, say): each side's sends go again once
                        // the other says what it has, on the next one.
                        (true, NetEvent::Closed { .. } | NetEvent::Unreachable { .. }) => down.down(),
                        (false, NetEvent::Closed { .. } | NetEvent::Unreachable { .. }) => {
                            up.down();
                            reconnects += 1;
                            agent.command(Out::Connect { addr }).await;
                        }
                        (server, NetEvent::Frame { data, .. }) => {
                            let frame = if server {
                                match crate::proto::decode::<AgentMsg>(&data)? {
                                    AgentMsg::Wire(f) => f,
                                    other => anyhow::bail!("the runner sent {other:?}"),
                                }
                            } else {
                                match crate::proto::decode::<ServerMsg>(&data)? {
                                    ServerMsg::Wire(f) => f,
                                    other => anyhow::bail!("the server sent {other:?}"),
                                }
                            };
                            let (rx, took, tx, answered) = if server { (&mut up_in, &mut took_up, &mut down, &mut answered_down) } else { (&mut down_in, &mut took_down, &mut up, &mut answered_up) };
                            match frame {
                                Frame::Item { stream, seq, low, body } => match rx.on_item(&stream, seq, low, body) {
                                    Arrival::Take(body) => {
                                        if let Some(t) = index(&body).and_then(|i| took.get_mut(i)) {
                                            *t += 1;
                                        }
                                        answers.push((server, rx.took(&stream, seq)));
                                    }
                                    Arrival::Answered(Some(a)) | Arrival::Gap(a) => answers.push((server, a)),
                                    Arrival::Answered(None) => {}
                                },
                                other => {
                                    for said in tx.on_frame(other) {
                                        if let Said::Answered { outcome, .. } = said {
                                            anyhow::ensure!(outcome == Outcome::Taken, "refused: {outcome:?}");
                                            *answered += 1;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                for (server, f) in answers {
                    match (server, peer) {
                        (true, Some(p)) => {
                            srv.send(p, LANE_CONTROL, Kind::Control, crate::proto::encode(&ServerMsg::Wire(f))?).await;
                        }
                        (false, _) => {
                            agent.send(SERVER_PEER, LANE_TERM, Kind::Term, crate::proto::encode(&AgentMsg::Wire(f))?).await;
                        }
                        (true, None) => {}
                    }
                }
            }
            anyhow::ensure!(took_up.iter().all(|n| *n == 1), "updates taken: {took_up:?}");
            anyhow::ensure!(took_down.iter().all(|n| *n == 1), "commands taken: {took_down:?}");
            anyhow::ensure!(chaos(LANE_TERM) > term0 && chaos(LANE_CONTROL) > control0, "each side counted what it dropped");
            Ok(())
        })
    }

    #[test]
    fn agent_and_server_exchange_frames_and_survive_rebuilds() -> anyhow::Result<()> {
        let _ = tracing_subscriber::fmt().with_env_filter("debug").with_test_writer().try_init();
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let pki = crate::pki::Pki::open(dir.path())?;
            let server_leaf = pki.server_leaf(&[])?;
            let probe = std::net::UdpSocket::bind("127.0.0.1:0")?;
            let addr = probe.local_addr()?;
            drop(probe);
            let (srv, srv_ev) = start(Spec { bind: addr, ca_der: pki.ca_der.clone(), leaf: server_leaf, server: true, dial_deadline: DIAL_DEADLINE })?;
            let agent_leaf = pki.issue_local("bench-1")?;
            let (agent, agent_ev) = start(Spec {
                bind: "127.0.0.1:0".parse()?,
                ca_der: pki.ca_der.clone(),
                leaf: agent_leaf,
                server: false,
                dial_deadline: Duration::from_secs(2),
            })?;

            // A dial into the void (nothing listens there) is abandoned at the deadline and
            // reported, instead of pending forever.
            let dead = std::net::UdpSocket::bind("127.0.0.1:0")?.local_addr()?;
            agent.command(Out::Connect { addr: dead }).await;
            let t = Instant::now();
            match next_event(&agent_ev, Instant::now() + Duration::from_secs(10)).await? {
                NetEvent::Unreachable { reason, .. } => assert!(reason.contains("no answer"), "{reason}"),
                other => anyhow::bail!("expected Unreachable, got {other:?}"),
            }
            assert!(t.elapsed() >= Duration::from_secs(2) && t.elapsed() < Duration::from_secs(6));

            // Dialing the void again, then the real server: the pending dial is replaced at once.
            agent.command(Out::Connect { addr: dead }).await;
            crate::rt::sleep(Duration::from_millis(300)).await;

            agent.command(Out::Connect { addr }).await;
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut agent_peer = None;
            // Each connection's binding, as the server and the agent computed it.
            let (mut srv_binding, mut agent_binding) = (None, None);
            let mut rebuilt = false;
            let mut logs = 0;
            let mut replies = 0;
            loop {
                let ev = futures::select_biased! {
                    ev = next_event(&srv_ev, deadline).fuse() => (true, ev?),
                    ev = next_event(&agent_ev, deadline).fuse() => (false, ev?),
                };
                match ev {
                    (true, NetEvent::Connected { peer, name, binding }) => {
                        assert_eq!(name.as_deref(), Some("bench-1"));
                        assert!(binding.is_some());
                        assert_ne!(binding, srv_binding, "a new connection has a new binding");
                        srv_binding = binding;
                        agent_peer = Some(peer);
                    }
                    (true, NetEvent::Frame { kind: Kind::Log, data, .. }) => {
                        assert_eq!(data.len(), 1 << 20);
                        logs += 1;
                    }
                    (true, NetEvent::Frame { peer, data, .. }) => {
                        assert_eq!(data, b"hello server");
                        srv.send(peer, 0, Kind::Control, b"hello agent".to_vec()).await;
                    }
                    (false, NetEvent::Connected { binding, .. }) => {
                        assert!(binding.is_some());
                        agent_binding = binding;
                        // Big log frame through the bulk lane, then a control frame.
                        agent.send(SERVER_PEER, 2, Kind::Log, vec![7u8; 1 << 20]).await;
                        agent.send(SERVER_PEER, 0, Kind::Control, b"hello server".to_vec()).await;
                    }
                    (false, NetEvent::Frame { data, .. }) => {
                        assert_eq!(data, b"hello agent");
                        assert!(agent_peer.is_some());
                        // Both ends derived the same binding for the connection that carried this.
                        assert_eq!(srv_binding, agent_binding);
                        replies += 1;
                    }
                    _ => {}
                }
                // Once the first connection carried both frames, rebuild while connected: the
                // agent must come back on its own (in-flight frames die with the old connection).
                if !rebuilt && replies >= 1 && logs >= 1 {
                    rebuilt = true;
                    agent.command(Out::Rebuild).await;
                }
                // Both connections delivered the control round trip and the 1 MiB bulk frame.
                if replies >= 2 && logs >= 2 {
                    return Ok(());
                }
            }
        })
    }
}

