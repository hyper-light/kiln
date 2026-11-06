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
//! What comes in waits for the owner in order while the loop goes on taking the owner's sends and
//! driving its connections (the owner waiting on the loop while the loop waited on the owner was a
//! deadlock). Past as much as a connection's receive window, the loop holds its peers' lanes
//! (`Endpoint::hold_lane`): what they send waits in QUIC, whose flow control holds them back, until
//! the owner has taken all it holds.
//!
//! Frames refused with `LaneFull` wait in a bounded per-lane outbox and are sent again once the
//! lane says it has room (`Event::LaneRoom`), or as the connection drains. A failure or panic inside the loop ends that
//! incarnation; the thread rebuilds the endpoint on a fresh runtime and an agent re-dials. The
//! owner can force the same rebuild with [`Out::Rebuild`] (the agent's dial watchdog does).
//!
//! Every frame the loop lets go before the wire is counted, by lane and reason
//! (`crate::metrics::NetMetrics`, docs/wire.md "Loss below the channel"). A control frame is never
//! shed: a control lane whose queue is full means its peer is stuck, and the connection is closed
//! instead, so the channel's resume repairs what it carried. Other lanes shed their oldest frame.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use futures::FutureExt as _;
use hyper_rt::runtime::LocalRuntime;
use hyper_rt_transport::{Driver, Io};
use hyper_transport::tls::{CertificateDer, Credentials, PrivateKeyDer, Tls};
use hyper_transport::{
    AdmissionLimits, AskQueue, Authenticated, Classes, Config, Directory, Endpoint, Event, Limits, PeerId,
    RECEIVE_CHUNK, Refusal, StreamCredit,
};

use crate::net_budget::Spares;
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
    /// Each peer's key (its SPKI's SHA-256, hex), and the keys revoked (W27-R01): recorded in the
    /// store, loaded at start, so a restart keeps them and re-enrolling a name doesn't undo them.
    /// Each revoked key maps to its refusal: when, why and by whom (`revoked at ... (why, by who)`).
    keys: HashMap<PeerId, String>,
    revoked_keys: HashMap<String, String>,
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
        let key = crate::pki::key_sha256(der)?;
        if let Some(why) = self.revoked_keys.get(&key) {
            tracing::warn!(agent = %name, key = %key, "refused a revoked certificate: {why}");
            return None;
        }
        // A machine named as the server (a certificate from before machines couldn't be) is none.
        if self.revoked.contains(&name) || crate::pki::reserved(&name) {
            return None;
        }
        let peer = peer_of(der);
        self.names.insert(peer, name);
        self.keys.insert(peer, key);
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
    /// Refuse certificates for these keys (SPKI SHA-256, hex) from now on, and drop the peers
    /// connected with them (W27-R01). Never undone: a machine enrolling again has a new key. `why`
    /// is the refusal said for each (`crate::identities::refusal`).
    RevokeKeys { keys: Vec<String>, why: String },
    /// Agent side: dial the server.
    Connect { addr: SocketAddr },
    /// Drop the endpoint and build a new one (fresh socket and runtime); an agent re-dials.
    Rebuild,
    /// A test build's fault (`crate::runner::chaos`: `shed=`, `twice=`, `late=`): frames dropped,
    /// sent twice or sent late as [`Shed`] says, from now on, beside the faults said before.
    Shed(Shed),
    /// A test build's seed (`crate::runner::chaos`, `seed=`): the faults said after it pick other
    /// frames than they would without it (each seed its own, the same each run).
    Seed(u64),
    /// What the endpoint measures now, answered on the loop's next turn ([`NetHandle::sample`]).
    Sample {
        reply: std::sync::mpsc::SyncSender<QuicSample>,
    },
    /// A test build's copy (`crate::runner::chaos`, `copy=`): every frame sent from now on is
    /// appended to this file as it's handed below the channel, before any fault (W27-R02).
    Copy(std::path::PathBuf),
}

/// About one frame in `every` of `lane` (of those whose first byte, a message's variant, is
/// `variant`, when it says) done to as `act` says before the wire: what goes wrong below the
/// channel. Which ones is a seeded draw, not every `every`th: a fixed period could fall on the
/// same item of each resend, as no real fault does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shed {
    pub lane: u32,
    pub variant: Option<u8>,
    pub every: u64,
    pub act: Act,
}

impl Shed {
    /// This rule as the sender of an enum of `variants` variants does it: an `unknown` fault's index
    /// one past them (in a byte: a postcard varint below 128).
    pub fn past(mut self, variants: usize) -> Shed {
        if let Act::Unknown(_) = self.act {
            self.act = Act::Unknown(u8::try_from(variants).unwrap_or(u8::MAX).min(127));
        }
        self
    }
}

/// What follows a frame's variant index once [`garble`] mangles it: ten bytes of 0xff. No message
/// of `AgentMsg`'s or `ServerMsg`'s reads as them (no varint ends in them, no bool, option or enum
/// has one as its tag, and one with no fields has them left over), so a mangled frame is always
/// malformed, never another message (`net::tests`).
pub const MANGLED: [u8; 10] = [0xff; 10];

/// `data` made unreadable as `draw` picks: cut short, bytes added after it, or what follows its
/// first byte (its variant index) mangled ([`MANGLED`]). Each can't be read: none is another
/// message its receiver would take.
fn garble(data: &mut Vec<u8>, draw: u64) {
    match draw % 3 {
        0 => data.truncate(data.len() / 2),
        1 => data.extend_from_slice(&[0xde, 0xad, 0xbe]),
        _ => {
            data.truncate(1);
            data.extend_from_slice(&MANGLED);
        }
    }
}

/// A test build's mark on a frame its faults made unreadable ([`Act::Unknown`], [`Act::Garble`]),
/// ahead of it on the wire. Its reader (a test build too, `crate::proto::read`) takes it off and
/// says the frame came, as it reads it, so a lab counts what came against what was refused
/// (`tests/wire_lab.rs`). No frame kiln sends begins with it: postcard never writes 0xff then 0x00.
pub const MARK: [u8; 8] = [0xff, 0x00, b'u', b'n', b'r', b'e', b'a', b'd'];

/// `frame` without its [`MARK`] when it came with one, said: a frame made unreadable, which its
/// reader is about to refuse.
#[cfg(any(test, feature = "chaos"))]
pub fn unmarked(frame: &[u8]) -> &[u8] {
    match frame.strip_prefix(&MARK) {
        Some(rest) => {
            tracing::warn!("chaos: an unreadable frame came");
            rest
        }
        None => frame,
    }
}

/// What a test build's fault does to a frame it picks ([`Shed`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Act {
    /// Dropped, counted as `chaos`: a frame lost below the channel.
    #[default]
    Drop,
    /// Sent twice.
    Twice,
    /// Held back and sent after the next frame on its lane, or once [`LATE_FOR`] passes with
    /// none: frames out of order.
    Late,
    /// Its first byte, its message's variant index, made this one: one past what its sender's enum
    /// has, a kind its receiver doesn't know (docs/wire.md, "What can't be read"). 0 until
    /// [`Shed::past`] says whose enum.
    Unknown(u8),
    /// Cut short, given bytes after it, or what follows its variant index mangled, as the same draw
    /// says: a frame that can't be read.
    Garble,
}

/// How long a frame sent late waits for the next on its lane, at the most.
pub const LATE_FOR: Duration = Duration::from_millis(20);

/// What a node's QUIC endpoint measures now ([`NetHandle::sample`]): its own counts, and each
/// connected peer's path and connection, as hyper-transport and hyper-quic keep them.
#[derive(Clone, Debug, Default)]
pub struct QuicSample {
    /// Connections held, in any state.
    pub connections: u64,
    /// Exchanges (a message each way, each on a stream) open.
    pub exchanges: u64,
    /// Streams a peer opened past a bound, refused, since the endpoint was made.
    pub streams_refused: u64,
    /// Lane frames skipped because their class or the budget could not take them, since then.
    pub frames_dropped: u64,
    /// Endpoint responses dropped at their bound, since then.
    pub responses_dropped: u64,
    /// Each peer with a connection: its name, its path now and what its connection counted.
    pub peers: Vec<PeerSample>,
}

/// A connected peer's path now and what its connection counted since it began.
#[derive(Clone, Debug)]
pub struct PeerSample {
    /// Its name: a runner's, or `server` for an agent's one peer.
    pub name: String,
    pub path: hyper_transport::PathFacts,
    pub conn: hyper_quic::ConnectionStats,
}

/// The peers a directory knows by name.
trait Named {
    fn each(&self, f: &mut dyn FnMut(PeerId, &str));
}

impl Named for ServerBook {
    fn each(&self, f: &mut dyn FnMut(PeerId, &str)) {
        for (peer, name) in &self.names {
            f(*peer, name);
        }
    }
}

impl Named for AgentBook {
    fn each(&self, f: &mut dyn FnMut(PeerId, &str)) {
        f(SERVER_PEER, "server");
    }
}

/// What `ep` measures now: its counts, and each named peer it has a connection to.
fn sample<D: Directory<Role = Role> + Named>(ep: &Endpoint<Kiln, Spares, D>) -> QuicSample {
    let st = ep.stats();
    let mut out = QuicSample {
        connections: st.connections as u64,
        exchanges: st.exchanges as u64,
        streams_refused: st.streams_refused,
        frames_dropped: st.frames_dropped,
        responses_dropped: st.responses_dropped,
        peers: Vec::new(),
    };
    ep.directory().each(&mut |peer, name| {
        if let (Some(path), Some(conn)) = (ep.path(peer), ep.connection_stats(peer)) {
            out.peers.push(PeerSample { name: name.to_owned(), path, conn });
        }
    });
    out
}

/// Events out of the net thread.
#[derive(Debug)]
pub enum NetEvent {
    /// `binding`: the connection's TLS exporter value under [`BINDING_LABEL`], the same at both
    /// ends of one connection and unknowable to anyone else (see `crate::tunnel`).
    Connected { peer: PeerId, name: Option<String>, binding: Option<[u8; 32]> },
    Closed { peer: PeerId, reason: String },
    Unreachable { peer: PeerId, reason: String },
    /// `at`: when the network thread took it off the link, so the hub can say how long it waited
    /// before it was handled (`kiln_route_wait_seconds{class="runner"}`, `crate::route_latency`).
    Frame { peer: PeerId, lane: u32, kind: Kind, data: Vec<u8>, at: std::time::Instant },
}

#[derive(Clone)]
pub struct NetHandle {
    tx: crate::memory::queue::Sender<Out>,
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

    /// What the endpoint measures now (its counts, each peer's path and connection), asked of the
    /// net thread and answered on its next turn; none if it does not answer `within`. Blocks: for a
    /// thread of its own, never the runtime's.
    pub fn sample(&self, within: Duration) -> Option<QuicSample> {
        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        self.tx.try_send(Out::Sample { reply }).ok()?;
        answer.recv_timeout(within).ok()
    }
}

/// What making a message's frames took at the slowest seen ([`send_made`]): half a mebibyte's in 1.4 ms on the CPU,
/// in a poll (rt9's lab, the server's cluster and observe loops on their shards). Bytes, then nanoseconds.
const MADE_SLOWEST: (u64, u64) = (512 * 1024, 1_400_000);

/// The most bytes of messages [`send_made`] makes into frames where it is, on its shard, not on the compute pool: what
/// making at the slowest seen ([`MADE_SLOWEST`]) does within a slow poll (`hyper_rt::slow::SLOW_POLL_NS`, 100 µs), 37,449
/// bytes. A batch of more goes to the pool as before (goal 9, plan 01a1a87b2cec step 5: every batch did, though most are
/// one write's few short messages to the cluster group's learners, a few hundred bytes each, whose hand-off, a wake of a
/// pool's thread, the shard's park while it waits and its wake when the job is done, cost the shard several times what
/// copying them does, once for every write kiln.db takes).
pub const MADE_HERE_MOST: usize = (hyper_rt::slow::SLOW_POLL_NS as u128 * MADE_SLOWEST.0 as u128 / MADE_SLOWEST.1 as u128) as usize;

/// What making the frames of `bytes` of messages is expected to take at the slowest seen ([`MADE_SLOWEST`]), nanoseconds.
fn made_ns(bytes: usize) -> u64 {
    u64::try_from(bytes).unwrap_or(u64::MAX).saturating_mul(MADE_SLOWEST.1) / MADE_SLOWEST.0
}

/// The messages `out` gives, sent in the order it gives them, each message's frames in theirs (goal 9, plan
/// 01a1a87b2cec step 3): each routed where it comes in (`route`: the peer it goes to and what its frames are made of;
/// none, and it's dropped), its frames made (`make`), and each handed to `send`. A frame is a copy of its message's
/// bytes as postcard writes them, up to half a mebibyte (the cluster group's and observe's lanes), which made on a
/// shard held it 0.3 to 1.4 ms on the CPU in a poll (rt9's lab: the server's cluster and observe loops): a batch of
/// messages is made on the compute pool, unless what `weigh` says they copy is within [`MADE_HERE_MOST`], when it's
/// made where it is, its turn ended first if that wouldn't fit it (`rt::Turns::before`; goal 9, plan 01a1a87b2cec step
/// 5). The messages waiting behind one are taken with it while the loop's turn lasts, and between sends the shard is
/// given up once the turn is over, whether or not other work wants it (`rt::Turns::end`), so each of its polls ends
/// within the quantum. Returns once `out` is closed and empty. `out` is `crate::memory::queue`'s, whose waits take no
/// lock (goal 5, plan 01a1a59c2aae, its top goal: an async-channel's took event-listener's `Mutex`).
pub async fn send_made<M, X, F, S, Fut>(out: &crate::memory::queue::Receiver<M>, mut route: impl FnMut(M) -> Option<(PeerId, X)>, weigh: impl Fn(&X) -> usize, make: F, mut send: S)
where
    X: Send + 'static,
    F: Fn(X) -> Vec<Vec<u8>> + Send + Sync + 'static,
    S: FnMut(PeerId, Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let make = Arc::new(make);
    let mut turns = crate::rt::Turns::new();
    loop {
        // One waiting already goes on in this turn; one waited for starts a turn afresh, the wait having given the
        // shard to its other work (as `rt::Turns::next` takes a queue's).
        let first = match out.try_recv() {
            Ok(m) => m,
            Err(async_channel::TryRecvError::Closed) => return,
            Err(async_channel::TryRecvError::Empty) => match out.recv().await {
                Ok(m) => {
                    turns = crate::rt::Turns::new();
                    m
                }
                Err(_) => return,
            },
        };
        let mut batch = Vec::new();
        // What making the batch's frames copies.
        let mut bytes = 0usize;
        let mut next = Some(first);
        while let Some(m) = next.take() {
            if let Some((peer, x)) = route(m) {
                bytes = bytes.saturating_add(weigh(&x));
                batch.push((peer, x));
            }
            if !turns.over() {
                next = out.try_recv().ok();
            }
        }
        if !batch.is_empty() {
            let made = if bytes <= MADE_HERE_MOST {
                turns.before(made_ns(bytes)).await;
                Ok(batch.into_iter().map(|(peer, x)| (peer, (*make)(x))).collect::<Vec<_>>())
            } else {
                let make = Arc::clone(&make);
                let made = crate::rt::compute_map(batch, move |(peer, x)| (peer, (*make)(x))).await;
                // The wait for the pool ended the poll: the sends start a turn of their own.
                turns = crate::rt::Turns::new();
                made
            };
            match made {
                Ok(made) => {
                    for (peer, frames) in made {
                        for frame in frames {
                            send(peer, frame).await;
                            turns.end().await;
                        }
                    }
                }
                // A job that unwound (a bug where they're made): its messages are lost, as a link that drops them
                // loses them, and their senders' own repair sends what's needed again.
                Err(e) => tracing::warn!(error = %format!("{e:#}"), "messages' frames weren't made: dropped"),
            }
        }
        turns.end().await;
    }
}

/// Per-peer, per-lane frames refused with `LaneFull`, bounded.
const OUTBOX_FRAMES: usize = 8192;

/// A connection's receive window (`Limits::window_ceiling`): the most of one peer's bytes the transport
/// lets be unread at once (QUIC's flow control, RFC 9000 §4.1). The net thread's queues are bounded by it
/// ([`EVENTS_BYTES`], [`OUT_BYTES`]).
pub const WINDOW: u64 = 64 << 20;

pub fn limits(server: bool) -> Limits {
    let window: u64 = WINDOW;
    Limits {
        admission: AdmissionLimits {
            pending: if server { 512 } else { 8 },
            identities: if server { 4096 } else { 4 },
            connections: if server { 4096 } else { 4 },
            per_identity: 2,
        },
        streams_per_connection: 64,
        // kiln opens no exchanges (lanes only): every connection holds its streams from its
        // handshake, and the exchange table alone bounds what would wait for one.
        stream_credit: StreamCredit::Fixed,
        ask_queue: AskQueue::Table,
        // A handshake from an address not yet proven is answered with a Retry once half the
        // pending places are taken (RFC 9000 §8.1.2), as before: kiln's places are a count, not
        // derived from its memory.
        retry_bytes: None,
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
        // As every connection asked before the choice was its owner's: decimated, as Google QUIC's
        // receivers acknowledge (hyper-raft docs/transport.md §4k, §4ak).
        acknowledgement: hyper_transport::Acknowledgement::Decimated,
        // No forward erasure correction, as before it was offered (§4an).
        fec_window: None,
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
    /// Server side: the keys (SPKI SHA-256, hex) whose certificates are refused from the start
    /// (W27-R01, `crate::identities::revoked_keys`), each with its refusal.
    pub revoked_keys: Vec<(String, String)>,
}

/// How long an agent's dial may go unanswered (a server that is down, or not routable yet at
/// boot, drops the handshake silently; QUIC alone would keep the dial pending).
pub const DIAL_DEADLINE: Duration = Duration::from_secs(15);

/// The bytes of transport events the net thread hands its owner (the hub's loop) at most before it holds
/// them itself (goal 11, plan 01a1a87b2e56 step 3): one connection's receive window ([`WINDOW`]), what the
/// net thread then holds itself before it holds its peers' lanes (`held_most`), so the owner's queue and
/// the net thread's together never hold more than two windows of what runners sent. Counted in bytes
/// ([`weight`]), never in events: 16,384 places of an event, made as the queue was, held 1.3 MB of an idle
/// server's heap (research 01a1a87b2e57, f10) and bounded nothing of the frames they carried.
pub const EVENTS_BYTES: usize = WINDOW as usize;

/// The bytes of commands (frames to send, mostly) waiting for the net thread, at most (goal 11, plan
/// 01a1a87b2e56 step 3): one connection's receive window ([`WINDOW`]), the most a peer takes unread, past
/// which whoever sends waits ([`NetHandle::send`]) or is told it's full ([`NetHandle::try_send`]). Its
/// 16,384 places took 0.9 MB of an idle server's heap made up front (research 01a1a87b2e57, f10).
pub const OUT_BYTES: usize = WINDOW as usize;

/// Start a net thread. Returns the command handle and the event stream.
pub fn start(spec: Spec) -> anyhow::Result<(NetHandle, crate::memory::queue::Receiver<NetEvent>)> {
    start_bounded(spec, OUT_BYTES, EVENTS_BYTES)
}

/// [`start`], its queues bounded at `out` and `events` bytes (tests: a bound reached without sending
/// two windows of frames).
pub fn start_bounded(spec: Spec, out: usize, events: usize) -> anyhow::Result<(NetHandle, crate::memory::queue::Receiver<NetEvent>)> {
    let (etx, erx) = crate::memory::queue::bounded::<NetEvent>(&crate::memory::owners::NET_EVENTS, events);
    Ok((start_with(spec, out, Box::new(etx))?, erx))
}

/// [`start`], what comes in handed to `hand`: the server's hub loops, one a shard (`crate::wire::fan`; goal 10,
/// plan 01a1a87b2dae step 4). Its commands bounded at `out` bytes.
pub fn start_with(spec: Spec, out: usize, hand: Box<dyn Hand>) -> anyhow::Result<NetHandle> {
    // Validate the TLS material up front so misconfiguration fails loudly at startup.
    let _ = Tls::mutual(&credentials(&spec.ca_der, &spec.leaf)?, spec.server)
        .map_err(|r| anyhow::anyhow!("tls config: {r}"))?;
    let (tx, rx) = crate::memory::queue::bounded::<Out>(&crate::memory::owners::NET_OUT, out);
    let name = if spec.server { "kiln-net-server" } else { "kiln-net-agent" };
    std::thread::Builder::new().name(name.into()).spawn(move || thread_main(spec, rx, hand))?;
    Ok(NetHandle { tx })
}

/// An event being handed to the loop's owner: kept across turns of the loop, as a send dropped
/// before it's done loses what it carried. True once taken; false when the owner is gone.
pub type Handing = std::pin::Pin<Box<dyn std::future::Future<Output = bool>>>;

/// Who takes what the net thread hands over: its owner's one queue ([`start`]), or the server's hub
/// loops, each event to the loop that takes it (`crate::wire::fan`; goal 10, plan 01a1a87b2dae step 4).
pub trait Hand: Send {
    /// `ev`, handed over now if there's room for it; else its handing once there is, or none when
    /// whoever takes it is gone.
    fn hand(&mut self, ev: NetEvent) -> Result<(), Option<Handing>>;
}

impl Hand for crate::memory::queue::Sender<NetEvent> {
    fn hand(&mut self, ev: NetEvent) -> Result<(), Option<Handing>> {
        match self.try_send(ev) {
            Ok(()) => Ok(()),
            Err(async_channel::TrySendError::Full(ev)) => {
                let tx = self.clone();
                Err(Some(Box::pin(async move { tx.send(ev).await.is_ok() })))
            }
            Err(async_channel::TrySendError::Closed(_)) => Err(None),
        }
    }
}

/// What the net thread hands its events to, kept across its loop's incarnations: the thread's alone.
type Owner = std::rc::Rc<std::cell::RefCell<Box<dyn Hand>>>;

/// What an event takes of memory: its place and, a frame, its bytes.
fn weight(ev: &NetEvent) -> usize {
    std::mem::size_of::<NetEvent>().saturating_add(match ev {
        NetEvent::Frame { data, .. } => data.len(),
        NetEvent::Connected { name, .. } => name.as_ref().map_or(0, String::len),
        NetEvent::Closed { reason, .. } | NetEvent::Unreachable { reason, .. } => reason.len(),
    })
}

impl crate::memory::queue::Weigh for NetEvent {
    fn weight(&self) -> usize {
        weight(self)
    }
}

impl crate::memory::queue::Weigh for Out {
    /// Its place and what it carries: a frame's bytes, the names and keys it says.
    fn weight(&self) -> usize {
        std::mem::size_of::<Out>().saturating_add(match self {
            Out::Frame { data, .. } => data.len(),
            Out::Revoke { name } | Out::Unrevoke { name } => name.len(),
            Out::RevokeKeys { keys, why } => keys.iter().map(String::len).sum::<usize>().saturating_add(why.len()),
            Out::Copy(path) => path.as_os_str().len(),
            Out::Disconnect { .. } | Out::Connect { .. } | Out::Rebuild | Out::Shed(_) | Out::Seed(_) | Out::Sample { .. } => 0,
        })
    }
}

/// Hands `held` to the owner in order: as many as it has room for, the next one waiting in
/// `handing` when it has no more. False when the owner is gone.
fn hand_over(owner: &Owner, held: &mut VecDeque<NetEvent>, held_bytes: &mut usize, handing: &mut Option<Handing>) -> bool {
    // Taken here alone, on this thread, and never across an await: what's held waits a turn should it be.
    let Ok(mut hand) = owner.try_borrow_mut() else { return true };
    while handing.is_none() {
        let Some(ev) = held.pop_front() else { break };
        *held_bytes = held_bytes.saturating_sub(weight(&ev));
        match hand.hand(ev) {
            Ok(()) => {}
            Err(Some(h)) => *handing = Some(h),
            Err(None) => return false,
        }
    }
    true
}

/// Holds each of `peers`' lanes (every lane a peer may open), or lets them go
/// ([`Endpoint::hold_lane`]).
fn hold_lanes<B, D>(ep: &mut Endpoint<Kiln, B, D>, peers: impl Iterator<Item = PeerId>, lanes: u32, hold: bool)
where
    B: hyper_transport::Budget<Class>,
    D: Directory<Role = Role>,
{
    for peer in peers {
        for lane in 0..lanes {
            let _ = if hold { ep.hold_lane(peer, lane) } else { ep.release_lane(peer, lane) };
        }
    }
}

/// How one incarnation of the loop ended.
enum Exit {
    /// The command channel closed: the owner is gone.
    Closed,
    /// Start over on a fresh endpoint; `redial`: an agent dials its last address at once. `carry`:
    /// what the owner hasn't taken yet, for the next incarnation to hand it.
    Rebuild { redial: bool, carry: Carry },
}

/// What one incarnation of the loop hands the next (goal 3, plan 01a1a59c2a8e step 5): the events
/// the owner had no room for yet, in order, and the one being handed to it. A rebuild used to drop
/// them, and the dial's deadline waited for room to say it went unanswered: the loop took no command
/// while it waited (perf/api-pass's deadlock, in its last place).
#[derive(Default)]
struct Carry {
    held: VecDeque<NetEvent>,
    held_bytes: usize,
    handing: Option<Handing>,
}

impl Carry {
    /// `held` and `handing` as they stand, with `events` (this turn's, not yet held) after them.
    fn of(mut held: VecDeque<NetEvent>, mut held_bytes: usize, handing: Option<Handing>, events: &mut Vec<NetEvent>) -> Carry {
        for ev in events.drain(..) {
            held_bytes = held_bytes.saturating_add(weight(&ev));
            held.push_back(ev);
        }
        Carry { held, held_bytes, handing }
    }
}

fn thread_main(spec: Spec, rx: crate::memory::queue::Receiver<Out>, hand: Box<dyn Hand>) {
    let spec = Arc::new(spec);
    let owner: Owner = std::rc::Rc::new(std::cell::RefCell::new(hand));
    let last_dial: Arc<Mutex<Option<SocketAddr>>> = Arc::new(Mutex::new(None));
    // One shard: the loop, plus helpers for any wait polled under a foreign waker.
    let config = crate::rt::runtime_config(1, 1024);
    let mut backoff = Duration::from_millis(200);
    // What the last incarnation hadn't handed the owner yet: the next hands it first.
    let mut carry = Carry::default();
    loop {
        let started = Instant::now();
        // A fresh runtime per incarnation: whatever a failed loop left behind goes with it.
        let outcome = match LocalRuntime::new(&config) {
            Ok(mut local) => local
                .block_on(run_loop(Arc::clone(&spec), rx.clone(), std::rc::Rc::clone(&owner), Arc::clone(&last_dial), std::mem::take(&mut carry)))
                .map_err(|e| format!("{e}")),
            Err(e) => Err(format!("cannot start the net runtime: {e}")),
        };
        match outcome {
            Ok(Ok(Exit::Closed)) => return,
            Ok(Ok(Exit::Rebuild { redial, carry: next })) => {
                carry = next;
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

type ServerDriver = Driver<Kiln, Spares, ServerBook>;
type AgentDriver = Driver<Kiln, Spares, AgentBook>;

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
    // Its spares bounded in bytes (goal 11, plan 01a1a87b2e56 step 5: `crate::net_budget`).
    let budget = Spares::new(if spec.server { 1 << 30 } else { 256 << 20 }, 256);
    let io = Io { batch: 64 };
    let now = Instant::now();
    if spec.server {
        let cfg = Config { tls, role: Role::Server, limits: limits(true), congestion: Default::default() };
        let book = ServerBook { revoked_keys: spec.revoked_keys.iter().cloned().collect(), ..ServerBook::default() };
        let ep = Endpoint::<Kiln, _, _>::new(cfg, budget, book, now)
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
    /// A test build's faults, each with its draw's state, and the seed they draw from.
    faults: Vec<(Shed, u64)>,
    seed: u64,
    /// A frame each lane holds back to send late (`Act::Late`), and since when.
    late: HashMap<(PeerId, u32), (Kind, Vec<u8>, Instant)>,
    /// A test build's copy of every frame handed to it (`Out::Copy`).
    copy: Option<std::fs::File>,
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
    fn push(&mut self, peer: PeerId, lane: u32, kind: Kind, mut data: Vec<u8>) -> Push {
        if let Some(f) = &mut self.copy {
            use std::io::Write as _;
            let len = u64::try_from(data.len()).unwrap_or(u64::MAX);
            if let Err(e) = f.write_all(&lane.to_le_bytes()).and_then(|()| f.write_all(&len.to_le_bytes())).and_then(|()| f.write_all(&data)) {
                tracing::warn!(error = %e, "chaos: can't copy a frame");
            }
        }
        let (mut twice, mut late, mut unreadable) = (false, false, false);
        for (rule, draw) in &mut self.faults {
            if rule.lane != lane || !rule.variant.is_none_or(|v| data.first() == Some(&v)) {
                continue;
            }
            // xorshift64*: the same frames each run, at no fixed period.
            *draw ^= *draw << 13;
            *draw ^= *draw >> 7;
            *draw ^= *draw << 17;
            if draw.wrapping_mul(0x2545_f491_4f6c_dd1d).checked_rem(rule.every.max(1)) != Some(0) {
                continue;
            }
            match rule.act {
                Act::Drop => {
                    crate::metrics::net().dropped(peer, lane, "chaos", 1);
                    return Push::Queued;
                }
                Act::Twice => twice = true,
                Act::Late => late = true,
                Act::Unknown(index) => {
                    if let Some(first) = data.first_mut() {
                        *first = index;
                    }
                    unreadable = true;
                }
                Act::Garble => {
                    garble(&mut data, *draw);
                    unreadable = true;
                }
            }
        }
        // Made unreadable: marked for its receiver to say it came, and said here, with how many
        // copies go.
        if unreadable {
            let mut marked = MARK.to_vec();
            marked.append(&mut data);
            data = marked;
            tracing::warn!(lane = crate::proto::lane_name(lane), copies = if twice { 2 } else { 1 }, "chaos: frames made unreadable");
        }
        // One held back goes after this one; this one waits for the next, when it's to be late.
        let held = self.late.remove(&(peer, lane));
        if late && held.is_none() {
            self.late.insert((peer, lane), (kind, data, Instant::now()));
            return Push::Queued;
        }
        let mut pushed = self.queue(peer, lane, kind, data.clone());
        if twice && pushed == Push::Queued {
            pushed = self.queue(peer, lane, kind, data);
        }
        if let Some((kind, data, _)) = held
            && pushed == Push::Queued
        {
            pushed = self.queue(peer, lane, kind, data);
        }
        pushed
    }

    /// Frames held back to send late whose next never came within [`LATE_FOR`]: sent now.
    fn release_late(&mut self, now: Instant) {
        let due: Vec<(PeerId, u32)> = self.late.iter().filter(|(_, (_, _, at))| now.saturating_duration_since(*at) >= LATE_FOR).map(|(k, _)| *k).collect();
        for (peer, lane) in due {
            if let Some((kind, data, _)) = self.late.remove(&(peer, lane)) {
                let _ = self.queue(peer, lane, kind, data);
            }
        }
    }

    fn queue(&mut self, peer: PeerId, lane: u32, kind: Kind, data: Vec<u8>) -> Push {
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
        self.late.is_empty() && self.queues.values().all(VecDeque::is_empty)
    }

    /// What fills `peer`'s `lane`: each message variant there (a frame's first byte) and how many
    /// of its frames, the most first, at most four.
    fn filled_with(&self, peer: PeerId, lane: u32) -> String {
        let mut n: HashMap<u8, usize> = HashMap::new();
        for (_, data) in self.queues.get(&(peer, lane)).into_iter().flatten() {
            *n.entry(data.first().copied().unwrap_or(0)).or_default() += 1;
        }
        let mut most: Vec<(u8, usize)> = n.into_iter().collect();
        most.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        most.iter().take(4).map(|(v, c)| format!("{v}×{c}")).collect::<Vec<_>>().join(", ")
    }

    /// Lets go of what's queued for `peer`, counted as dropped for `reason`.
    fn forget(&mut self, peer: PeerId, reason: &'static str) {
        self.late.retain(|(p, _), _| *p != peer);
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
    // A test build's frames held back to go late whose next never came: they go now.
    outbox.release_late(Instant::now());
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
    rx: crate::memory::queue::Receiver<Out>,
    owner: Owner,
    last_dial: Arc<Mutex<Option<SocketAddr>>>,
    carry: Carry,
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
    // What the owner has no room for yet, in order, and the one being handed to it now. The loop
    // goes on meanwhile, taking commands (the owner's sends wait for this loop to take them: the
    // owner waiting on this loop while this loop waited on the owner was a deadlock, and every
    // connection went quiet as it lasted) and driving its connections, its sends among them. Once
    // it holds as much for the owner as it lets any one peer have unread (a connection's receive
    // window, `Limits::window_ceiling`), it holds its peers' lanes: what they send waits in QUIC,
    // whose flow control holds them back (RFC 9000 §4.1), until the owner has taken all of it.
    // An incarnation after a rebuild starts with what the last hadn't handed over.
    let Carry { mut held, mut held_bytes, mut handing } = carry;
    let (held_most, lanes) = {
        let l = limits(spec.server);
        (usize::try_from(l.window_ceiling).unwrap_or(usize::MAX), l.lanes_per_peer)
    };
    let mut peers: HashSet<PeerId> = HashSet::new();
    let mut holding = false;

    enum Ev {
        Cmd(Option<Out>),
        Driver(Result<Event<Kiln>, hyper_rt::RtError>),
        Retry,
        DialDeadline,
        Handed(bool),
    }

    loop {
        if !hand_over(&owner, &mut held, &mut held_bytes, &mut handing) {
            return Ok(Exit::Closed);
        }
        let hold = if holding { handing.is_some() || !held.is_empty() } else { held_bytes >= held_most };
        if hold != holding {
            with_driver!(&mut driver, d => hold_lanes(d.endpoint(), peers.iter().copied(), lanes, hold));
            holding = hold;
        }
        let pending = !outbox.is_empty();
        let ev = {
            let cmd = rx.recv().fuse();
            // `event()` is cancel-safe: dropping it when another branch wins loses nothing.
            let drv = async { with_driver!(&mut driver, d => d.event().await) }.fuse();
            let handed = async {
                match handing.as_mut() {
                    Some(h) => h.await,
                    None => futures::future::pending::<bool>().await,
                }
            }
            .fuse();
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
            futures::pin_mut!(cmd, drv, retry, dial_timer, handed);
            futures::select_biased! {
                h = handed => Ev::Handed(h),
                c = cmd => Ev::Cmd(c.ok()),
                e = drv => Ev::Driver(e),
                () = dial_timer => Ev::DialDeadline,
                () = retry => Ev::Retry,
            }
        };
        match ev {
            Ev::Handed(taken) => {
                handing = None;
                if !taken {
                    return Ok(Exit::Closed);
                }
            }
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
                                tracing::warn!(peer, lane, frames = OUTBOX_FRAMES, filled_with = %outbox.filled_with(peer, lane), "a control lane's queue is full, its peer stuck: its connection closes");
                                crate::metrics::net().dropped(peer, lane, "stuck", 1);
                                outbox.forget(peer, "stuck");
                                with_driver!(&mut driver, d => {
                                    let now = clock!(d);
                                    d.endpoint().disconnect(now, peer);
                                });
                            }
                        }
                        Out::Shed(rule) => {
                            tracing::warn!(?rule, seed = outbox.seed, "chaos: frames go wrong below the channel");
                            // Each fault draws on its own, from a seed of its own (never 0, which
                            // xorshift never leaves).
                            let seed = (0x9e37_79b9_7f4a_7c15_u64 ^ outbox.seed.wrapping_mul(0x94d0_49bb_1331_11eb) ^ (outbox.faults.len() as u64).wrapping_mul(0xbf58_476d_1ce4_e5b9)).max(1);
                            outbox.faults.push((rule, seed));
                        }
                        Out::Seed(seed) => outbox.seed = seed,
                        Out::Copy(path) => {
                            tracing::warn!(path = %path.display(), "chaos: every frame sent is copied");
                            outbox.copy = std::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(&path)
                                .map_err(|e| tracing::warn!(error = %e, path = %path.display(), "chaos: can't copy frames"))
                                .ok();
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
                        Out::Sample { reply } => {
                            let now = with_driver!(&mut driver, d => sample(d.endpoint()));
                            let _ = reply.try_send(now);
                        }
                        Out::RevokeKeys { keys, why } => {
                            if let AnyDriver::Server(d) = &mut driver {
                                let now = clock!(d);
                                let book = d.endpoint().directory_mut();
                                let peers: Vec<PeerId> = book.keys.iter().filter(|(_, k)| keys.contains(k)).map(|(p, _)| *p).collect();
                                // The first revocation of a key is the one it's refused for.
                                for k in keys {
                                    book.revoked_keys.entry(k).or_insert_with(|| why.clone());
                                }
                                for p in peers {
                                    d.endpoint().disconnect(now, p);
                                }
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
                                    Dial::Dialing { .. } | Dial::Connected => return Ok(Exit::Rebuild { redial: true, carry: Carry::of(held, held_bytes, handing, &mut events) }),
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
                        Out::Rebuild => return Ok(Exit::Rebuild { redial: true, carry: Carry::of(held, held_bytes, handing, &mut events) }),
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
                    // Said after what the owner hasn't taken yet, and carried: the next incarnation
                    // hands it over as the owner makes room, going on meanwhile (it used to wait here
                    // for room, taking no command). Then start over clean: the owner re-dials with
                    // its backoff.
                    events.push(NetEvent::Unreachable {
                        peer: SERVER_PEER,
                        reason: format!("no answer from {addr} in {}s", spec.dial_deadline.as_secs()),
                    });
                    return Ok(Exit::Rebuild { redial: false, carry: Carry::of(held, held_bytes, handing, &mut events) });
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
            match &ev {
                // One that connects while the others' lanes are held is held too.
                NetEvent::Connected { peer, .. } => {
                    peers.insert(*peer);
                    if holding {
                        with_driver!(&mut driver, d => hold_lanes(d.endpoint(), std::iter::once(*peer), lanes, true));
                    }
                }
                NetEvent::Closed { peer, .. } | NetEvent::Unreachable { peer, .. } => {
                    if holding {
                        with_driver!(&mut driver, d => hold_lanes(d.endpoint(), std::iter::once(*peer), lanes, false));
                    }
                    peers.remove(peer);
                }
                _ => {}
            }
            if !spec.server {
                match &ev {
                    NetEvent::Connected { peer, .. } if *peer == SERVER_PEER => dial = Dial::Connected,
                    NetEvent::Closed { peer, .. } | NetEvent::Unreachable { peer, .. } if *peer == SERVER_PEER => {
                        dial = Dial::Idle;
                    }
                    _ => {}
                }
            }
            held_bytes = held_bytes.saturating_add(weight(&ev));
            held.push_back(ev);
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
            out.push(NetEvent::Frame { peer, lane, kind, data: frame.bytes().to_vec(), at: std::time::Instant::now() });
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
        // A lane that found a frame too full has room again: the loop flushes the outbox after
        // every event it handles.
        // A path's degrading and its progress again (hyper-raft docs/transport.md §4ai): kiln's
        // links keep their own liveness, as before these were said.
        Event::BodyReady { .. } | Event::Writable { .. } | Event::LaneRoom { .. } | Event::Degrading { .. } | Event::Progressing { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal 9, plan 01a1a87b2cec step 3: what `send_made` sends is what came, in the order it came and each message's
    /// frames in theirs, less what had nowhere to go; the frames of messages long to make are made off the shards (on
    /// the compute pool); and no poll of it runs more sends than its turn has room for, though nothing else asks for its
    /// shard: each send here takes at least `SEND`, so `quantum / SEND` of them, rounded up, less one (the host holding
    /// the thread only makes sends longer, and turns shorter).
    #[test]
    fn g9_s3_frames_are_made_off_the_shards_and_sent_in_order_in_turns() -> anyhow::Result<()> {
        use std::future::Future as _;
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        crate::rt::block_on(async {
            const SEND: Duration = Duration::from_micros(20);
            let send_ns = u64::try_from(SEND.as_nanos()).unwrap_or(u64::MAX);
            let quantum_ns = hyper_rt::futures::step_budget_ns().unwrap_or(0);
            anyhow::ensure!(quantum_ns > 0, "a shard has a quantum");
            // Sends for several turns: two frames a message, a third of the messages with nowhere to go.
            let n = u32::try_from(quantum_ns.saturating_mul(8) / send_ns).unwrap_or(u32::MAX).max(64);
            let (tx, rx) = crate::memory::queue::unbounded::<u32>(&crate::memory::owners::CHANNELS);
            for m in 0..n {
                tx.try_send(m).map_err(|_| anyhow::anyhow!("the queue refused one"))?;
            }
            drop(tx);
            let route = |m: u32| (!m.is_multiple_of(3)).then_some((u64::from(m % 5), m));
            let made_on_a_shard = Arc::new(AtomicBool::new(false));
            let make = {
                let on = Arc::clone(&made_on_a_shard);
                move |m: u32| {
                    if crate::rt::on_shard() {
                        on.store(true, Ordering::SeqCst);
                    }
                    vec![m.to_be_bytes().to_vec(), vec![0xff]]
                }
            };
            let (sends, most_quantum_ns) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(quantum_ns)));
            let mut sent: Vec<(PeerId, Vec<u8>)> = Vec::new();
            let (mut polls, mut most_in_a_poll) = (0u64, 0u64);
            {
                let (counted, quantum) = (Arc::clone(&sends), Arc::clone(&most_quantum_ns));
                let send = |peer: PeerId, frame: Vec<u8>| {
                    let until = Instant::now() + SEND;
                    while Instant::now() < until {
                        std::hint::spin_loop();
                    }
                    // The quantum as the turn reads it (the shard's online estimate may move).
                    quantum.fetch_max(hyper_rt::futures::step_budget_ns().unwrap_or(0), Ordering::SeqCst);
                    counted.fetch_add(1, Ordering::SeqCst);
                    sent.push((peer, frame));
                    async { true }
                };
                // Each message weighed past what's made where it is ([`MADE_HERE_MOST`]): every batch goes to the pool.
                let mut work = std::pin::pin!(send_made(&rx, route, |_: &u32| MADE_HERE_MOST + 1, make, send));
                std::future::poll_fn(|cx| {
                    let before = sends.load(Ordering::SeqCst);
                    let polled = work.as_mut().poll(cx);
                    polls += 1;
                    most_in_a_poll = most_in_a_poll.max(sends.load(Ordering::SeqCst).saturating_sub(before));
                    polled
                })
                .await;
            }
            let expected: Vec<(PeerId, Vec<u8>)> = (0..n).filter(|m| !m.is_multiple_of(3)).flat_map(|m| [(u64::from(m % 5), m.to_be_bytes().to_vec()), (u64::from(m % 5), vec![0xff])]).collect();
            anyhow::ensure!(sent == expected, "sent {} frames, not the {} that came, in their order", sent.len(), expected.len());
            anyhow::ensure!(!made_on_a_shard.load(Ordering::SeqCst), "frames were made on a shard");
            let bound = most_quantum_ns.load(Ordering::SeqCst).div_ceil(send_ns).saturating_sub(1).max(1);
            anyhow::ensure!(most_in_a_poll <= bound, "a poll ran {most_in_a_poll} sends of at least {SEND:?}, past its turn's {bound}");
            anyhow::ensure!(polls > 1, "{} sends in one poll", sent.len());
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 5: a batch of short messages, whose making copies no more than [`MADE_HERE_MOST`],
    /// is made where it is, on its shard, with no hand-off to the compute pool and back; a batch past it still goes to the
    /// pool. Either way what's sent is what came, in the order it came, each message's frames in theirs.
    #[test]
    fn g9_s5_a_batch_of_short_messages_is_made_where_it_is() -> anyhow::Result<()> {
        use std::sync::atomic::{AtomicU64, Ordering};
        crate::rt::block_on(async {
            const MESSAGES: u32 = 16;
            // A cluster write's few hundred bytes a learner; and one message past the bound, alone or not.
            for (each, here) in [(512usize, true), (MADE_HERE_MOST + 1, false)] {
                let (tx, rx) = crate::memory::queue::unbounded::<u32>(&crate::memory::owners::CHANNELS);
                for m in 0..MESSAGES {
                    tx.try_send(m).map_err(|_| anyhow::anyhow!("the queue refused one"))?;
                }
                drop(tx);
                let (on_shard, off_shard) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
                let make = {
                    let (on, off) = (Arc::clone(&on_shard), Arc::clone(&off_shard));
                    move |m: u32| {
                        let made_on = if crate::rt::on_shard() { &on } else { &off };
                        made_on.fetch_add(1, Ordering::SeqCst);
                        vec![m.to_be_bytes().to_vec(), vec![0xff]]
                    }
                };
                let mut sent: Vec<(PeerId, Vec<u8>)> = Vec::new();
                let send = |peer: PeerId, frame: Vec<u8>| {
                    sent.push((peer, frame));
                    async { true }
                };
                send_made(&rx, |m: u32| Some((u64::from(m % 3), m)), move |_: &u32| each, make, send).await;
                let expected: Vec<(PeerId, Vec<u8>)> = (0..MESSAGES).flat_map(|m| [(u64::from(m % 3), m.to_be_bytes().to_vec()), (u64::from(m % 3), vec![0xff])]).collect();
                anyhow::ensure!(sent == expected, "{each} bytes a message: sent {} frames, not the {} that came, in their order", sent.len(), expected.len());
                let (on, off) = (on_shard.load(Ordering::SeqCst), off_shard.load(Ordering::SeqCst));
                let all = u64::from(MESSAGES);
                if here {
                    anyhow::ensure!((on, off) == (all, 0), "{each} bytes a message: {on} made on the shard, {off} off it, where every batch is made where it is");
                } else {
                    anyhow::ensure!((on, off) == (0, all), "{each} bytes a message: {on} made on the shard, {off} off it, where every batch goes to the pool");
                }
            }
            anyhow::ensure!(MADE_HERE_MOST == 37_449, "half a mebibyte in 1.4 ms, within 100 µs: {MADE_HERE_MOST}");
            anyhow::ensure!(made_ns(MADE_HERE_MOST) <= hyper_rt::slow::SLOW_POLL_NS, "the most made here is made within a slow poll at the slowest seen");
            Ok(())
        })
    }

    /// A frame garbled for a test can't be read: cut short, given bytes after it, or what follows
    /// its variant index mangled (its index stays).
    #[test]
    fn a_garbled_frame_is_cut_short_given_more_or_mangled_past_its_index() -> anyhow::Result<()> {
        let frame = crate::proto::encode(&crate::proto::AgentMsg::HarnessAdopted { session: "session".into(), error: Some("no room".into()) })?;
        let mut cut = frame.clone();
        garble(&mut cut, 3);
        assert!(cut.len() < frame.len() && crate::proto::read_agent(&cut).is_err());
        let mut more = frame.clone();
        garble(&mut more, 4);
        assert!(more.len() == frame.len() + 3 && crate::proto::read_agent(&more).is_err());
        for draw in [5, 8, 11, 302] {
            let mut mangled = frame.clone();
            garble(&mut mangled, draw);
            assert_eq!(mangled.first(), frame.first(), "its index stays");
            assert_eq!(mangled.get(1..), Some(&MANGLED[..]));
            assert!(crate::proto::read_agent(&mangled).is_err());
        }
        Ok(())
    }

    /// No message of either side's reads as a mangled frame of its variant: every one is refused,
    /// as `malformed`, so a lab's faults are never taken for a message (`tests/wire_lab.rs`).
    #[test]
    fn a_mangled_frame_of_any_variant_is_malformed() {
        use crate::wire::unread::Code;
        let read = |of: &str, n: usize, read: &dyn Fn(&[u8]) -> Result<(), crate::proto::Unreadable>| -> Vec<String> {
            (0..n)
                .filter_map(|index| {
                    let mut frame = vec![u8::try_from(index).unwrap_or(u8::MAX)];
                    frame.extend_from_slice(&MANGLED);
                    match read(&frame) {
                        Err(e) if e.code() == Code::Malformed => None,
                        Err(e) => Some(format!("{of} variant {index}: {e:?}")),
                        Ok(()) => Some(format!("{of} variant {index} read as a message")),
                    }
                })
                .collect()
        };
        let mut wrong = read("AgentMsg", crate::wire::agent_classes().len(), &|b| crate::proto::read_agent(b).map(drop));
        wrong.extend(read("ServerMsg", crate::wire::server_classes().len(), &|b| crate::proto::read_server(b).map(drop)));
        assert!(wrong.is_empty(), "{wrong:?}");
    }

    /// A frame a fault made unreadable goes marked, and its reader reads it as it was made: the
    /// mark comes off.
    #[test]
    fn a_frame_made_unreadable_goes_marked_and_is_read_unmarked() {
        let mut o = Outbox::default();
        let (peer, lane) = (7, crate::proto::LANE_TERM);
        // One past the runner's last message, as many as there are (W27's came after main's 60).
        let past = u8::try_from(crate::wire::agent_classes().len()).unwrap_or(u8::MAX);
        o.faults = vec![(Shed { lane, variant: Some(46), every: 1, act: Act::Unknown(past) }, 0x9e37_79b9_7f4a_7c15)];
        let frame = vec![46u8, 1, 2, 3];
        assert_eq!(o.push(peer, lane, Kind::Term, frame.clone()), Push::Queued);
        let sent = o.queues.get(&(peer, lane)).and_then(|q| q.front()).map(|(_, d)| d.clone()).unwrap_or_default();
        assert!(sent.starts_with(&MARK), "{sent:?}");
        assert_eq!(unmarked(&sent), [past, 1, 2, 3], "its index one past, the mark off");
        assert_eq!(unmarked(&frame), frame, "one not made unreadable is read as it went");
        assert!(matches!(crate::proto::read_agent(&sent), Err(crate::proto::Unreadable::Unknown { index, .. }) if index == u64::from(past)), "read as it was made");
    }

    async fn next_event(rx: &crate::memory::queue::Receiver<NetEvent>, deadline: Instant) -> anyhow::Result<NetEvent> {
        crate::rt::timeout(deadline.saturating_duration_since(Instant::now()), rx.recv())
            .await
            .map_err(|_| anyhow::anyhow!("timed out"))?
            .map_err(|_| anyhow::anyhow!("event stream closed"))
    }

    /// The server's next `Connected`, skipping other events; `None` once `within` passes.
    async fn connected_within(rx: &crate::memory::queue::Receiver<NetEvent>, within: Duration) -> Option<PeerId> {
        let deadline = Instant::now() + within;
        loop {
            match next_event(rx, deadline).await {
                Ok(NetEvent::Connected { peer, .. }) => return Some(peer),
                Ok(_) => {}
                Err(_) => return None,
            }
        }
    }

    /// W27-R01: a revoked key's certificate is dropped at once and refused when it comes back,
    /// and a server started with it revoked refuses it from the first handshake; another key of
    /// the same machine name (enrolled again) is welcome.
    #[test]
    fn w27_r01_revoked_keys_are_refused_from_the_moment_and_after_a_restart() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let pki = crate::pki::Pki::open_scratch(dir.path())?;
            let (old, new) = (pki.issue_local("fedora-2")?, pki.issue_local("fedora-2")?);
            let old_key = crate::pki::fingerprints(&old.cert_pem)?.key;
            let addr = |s: std::net::UdpSocket| s.local_addr();
            let a1 = addr(std::net::UdpSocket::bind("127.0.0.1:0")?)?;
            let (srv, srv_ev) = start(Spec { bind: a1, ca_der: pki.ca_der.clone(), leaf: pki.server_leaf(&[])?, server: true, dial_deadline: DIAL_DEADLINE, revoked_keys: Vec::new() })?;
            let (agent, _agent_ev) = start(Spec { bind: "127.0.0.1:0".parse()?, ca_der: pki.ca_der.clone(), leaf: old.clone(), server: false, dial_deadline: Duration::from_secs(2), revoked_keys: Vec::new() })?;
            agent.command(Out::Connect { addr: a1 }).await;
            let peer = connected_within(&srv_ev, Duration::from_secs(10)).await.ok_or_else(|| anyhow::anyhow!("never connected"))?;
            srv.command(Out::RevokeKeys { keys: vec![old_key.clone()], why: "revoked at 2026-10-26T21:00:00Z (runner forgotten, by ada)".into() }).await;
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                match next_event(&srv_ev, deadline).await? {
                    NetEvent::Closed { peer: p, .. } if p == peer => break,
                    _ => {}
                }
            }
            agent.command(Out::Connect { addr: a1 }).await;
            assert_eq!(connected_within(&srv_ev, Duration::from_secs(3)).await, None, "a revoked key comes back: refused");
            // A server started with the key revoked (a restart reading the store).
            let a2 = addr(std::net::UdpSocket::bind("127.0.0.1:0")?)?;
            let (_srv2, srv2_ev) = start(Spec { bind: a2, ca_der: pki.ca_der.clone(), leaf: pki.server_leaf(&[])?, server: true, dial_deadline: DIAL_DEADLINE, revoked_keys: vec![(old_key, "revoked at 2026-10-26T21:00:00Z (runner forgotten, by ada)".into())] })?;
            agent.command(Out::Connect { addr: a2 }).await;
            assert_eq!(connected_within(&srv2_ev, Duration::from_secs(3)).await, None, "refused from the first handshake");
            let (agent2, _ev2) = start(Spec { bind: "127.0.0.1:0".parse()?, ca_der: pki.ca_der.clone(), leaf: new, server: false, dial_deadline: Duration::from_secs(2), revoked_keys: Vec::new() })?;
            agent2.command(Out::Connect { addr: a2 }).await;
            assert!(connected_within(&srv2_ev, Duration::from_secs(10)).await.is_some(), "the same name with a new key connects");
            Ok(())
        })
    }

    /// A test build's fault sends the frames it picks twice, or late: after the next frame on its
    /// lane, or once `LATE_FOR` passes with none.
    #[test]
    fn a_fault_sends_frames_twice_or_late() {
        let (peer, bulk): (PeerId, u32) = (0xdead_0009, 13);
        let queued = |o: &Outbox| -> Vec<Vec<u8>> { o.queues.get(&(peer, bulk)).into_iter().flatten().map(|(_, d)| d.clone()).collect() };
        // Let go of uncounted: the lane is "other" to the metric, which the test after this one
        // counts on alone.
        let done = |mut o: Outbox| o.queues.clear();
        let seed = 0x9e37_79b9_7f4a_7c15;
        let mut o = Outbox::default();
        o.faults = vec![(Shed { lane: bulk, variant: Some(9), every: 1, act: Act::Twice }, seed)];
        o.push(peer, bulk, Kind::Log, vec![9, 1]);
        o.push(peer, bulk, Kind::Log, vec![8, 2]);
        assert_eq!(queued(&o), [vec![9, 1], vec![9, 1], vec![8, 2]], "a 9 twice, the 8 once");
        done(o);
        let mut o = Outbox::default();
        o.faults = vec![(Shed { lane: bulk, variant: Some(9), every: 1, act: Act::Late }, seed)];
        o.push(peer, bulk, Kind::Log, vec![9, 1]);
        assert!(queued(&o).is_empty() && !o.is_empty(), "held back, and the outbox not empty for it");
        o.push(peer, bulk, Kind::Log, vec![8, 2]);
        assert_eq!(queued(&o), [vec![8, 2], vec![9, 1]], "after the next");
        o.push(peer, bulk, Kind::Log, vec![9, 3]);
        o.release_late(Instant::now());
        assert_eq!(queued(&o).len(), 2, "not yet: its next may still come");
        o.release_late(Instant::now() + LATE_FOR);
        assert_eq!(queued(&o).last(), Some(&vec![9, 3]), "none came: it goes");
        done(o);
        // Both faults on one lane: drawn each on its own.
        let mut o = Outbox::default();
        o.faults = vec![(Shed { lane: bulk, variant: None, every: 2, act: Act::Twice }, seed), (Shed { lane: bulk, variant: None, every: 2, act: Act::Late }, seed ^ 0xbf58_476d_1ce4_e5b9)];
        for n in 0..200u8 {
            o.push(peer, bulk, Kind::Log, vec![n]);
        }
        o.release_late(Instant::now() + LATE_FOR);
        let got = queued(&o);
        let mut seen: Vec<u8> = got.iter().filter_map(|d| d.first().copied()).collect();
        let in_order = seen.windows(2).all(|w| w.first() <= w.get(1));
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 200, "none lost");
        assert!(got.len() > 200 && !in_order, "some twice ({}), some late", got.len());
        done(o);
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
        for i in 0..OUTBOX_FRAMES {
            assert_eq!(o.push(peer, control, Kind::Control, vec![if i == 0 { 5 } else { 2 }]), Push::Queued);
        }
        assert_eq!(o.push(peer, control, Kind::Control, vec![3]), Push::Stuck, "a full control lane sheds nothing");
        assert_eq!(o.filled_with(peer, control), format!("2×{}, 5×1", OUTBOX_FRAMES - 1), "what filled it, said by message variant");
        assert_eq!(grew(0, "shed"), 1);
        o.forget(peer, "closed");
        assert_eq!(grew(1, "closed"), 2 * OUTBOX_FRAMES as u64, "what a closed connection had queued, counted");
        // A test build's fault: every one of the bulk lane's frames whose first byte is 9.
        o.faults = vec![(Shed { lane: bulk, variant: Some(9), every: 1, act: Act::Drop }, 0x9e37_79b9_7f4a_7c15)];
        for b in [9, 8, 9, 9, 9, 9] {
            assert_eq!(o.push(peer, bulk, Kind::Log, vec![b]), Push::Queued);
        }
        assert_eq!(grew(2, "chaos"), 5);
        // One in three: some, not all, of 300.
        o.faults = vec![(Shed { lane: bulk, variant: None, every: 3, act: Act::Drop }, 0x9e37_79b9_7f4a_7c15)];
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

    /// Sends `N` updates up and `N` commands down kiln's real QUIC layer between a runner and a
    /// server, the runner's channel frames gone wrong as `agent_faults` say and the server's as
    /// `server_faults` do, the channel making up for it (a receiver's `Want`, a sender's patience):
    /// how many times each update and each command was taken, and in what order.
    fn through_faults(name: &str, agent_faults: &[Shed], server_faults: &[Shed]) -> anyhow::Result<Taken> {
        use crate::proto::{AgentMsg, LANE_CONTROL, LANE_TERM, ServerMsg};
        use crate::wire::channel::{Arrival, Event as Said, Frame, Outcome, Receiver, Sender};
        let (name, agent_faults, server_faults) = (name.to_owned(), agent_faults.to_vec(), server_faults.to_vec());
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let pki = crate::pki::Pki::open(dir.path())?;
            let probe = std::net::UdpSocket::bind("127.0.0.1:0")?;
            let addr = probe.local_addr()?;
            drop(probe);
            let (srv, srv_ev) = start(Spec { bind: addr, ca_der: pki.ca_der.clone(), leaf: pki.server_leaf(&[])?, server: true, dial_deadline: DIAL_DEADLINE, revoked_keys: Vec::new() })?;
            let (agent, agent_ev) = start(Spec { bind: "127.0.0.1:0".parse()?, ca_der: pki.ca_der.clone(), leaf: pki.issue_local(&name)?, server: false, dial_deadline: DIAL_DEADLINE, revoked_keys: Vec::new() })?;
            for f in agent_faults {
                agent.command(Out::Shed(f)).await;
            }
            for f in server_faults {
                srv.command(Out::Shed(f)).await;
            }
            agent.command(Out::Connect { addr }).await;
            let patience = Duration::from_millis(300);
            // Updates up (the runner's sender, the server's receiver); commands down.
            let (mut up, mut up_in) = (Sender::new(1 << 20, patience), Receiver::new());
            // The runner's of the server's commands asks for a gap again on time, as a runner does.
            let (mut down, mut down_in) = (Sender::new(1 << 20, patience), Receiver::timed());
            for n in 0..N {
                up.push("s:x", n.to_be_bytes().to_vec()).map_err(|_| anyhow::anyhow!("full"))?;
                down.push("cmd", n.to_be_bytes().to_vec()).map_err(|_| anyhow::anyhow!("full"))?;
            }
            let index = |body: &[u8]| body.try_into().ok().map(u64::from_be_bytes).and_then(|n| usize::try_from(n).ok());
            let (mut took_up, mut took_down) = (vec![0u32; N as usize], vec![0u32; N as usize]);
            let (mut order_up, mut order_down) = (Vec::new(), Vec::new());
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
                                match crate::proto::read_agent(&data).map_err(|e| anyhow::anyhow!("the runner's frame: {}: {}", e.what(), e.why("AgentMsg")))? {
                                    AgentMsg::Wire(f) => f,
                                    other => anyhow::bail!("the runner sent {other:?}"),
                                }
                            } else {
                                match crate::proto::read_server(&data).map_err(|e| anyhow::anyhow!("the server's frame: {}: {}", e.what(), e.why("ServerMsg")))? {
                                    ServerMsg::Wire(f) => f,
                                    other => anyhow::bail!("the server sent {other:?}"),
                                }
                            };
                            let (rx, took, tx, answered) = if server { (&mut up_in, &mut took_up, &mut down, &mut answered_down) } else { (&mut down_in, &mut took_down, &mut up, &mut answered_up) };
                            match frame {
                                Frame::Item { stream, seq, low, body } => match rx.on_item(&stream, seq, low, body, Instant::now()) {
                                    Arrival::Take(body) => {
                                        if let Some(i) = index(&body) {
                                            if let Some(t) = took.get_mut(i) {
                                                *t += 1;
                                            }
                                            if server { order_up.push(i) } else { order_down.push(i) }
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
                // The gaps the runner still lacks of the server's commands, asked for again on its
                // own clock.
                answers.extend(down_in.wants(Instant::now()).into_iter().map(|f| (false, f)));
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
            Ok(Taken { up: took_up, down: took_down, order_up, order_down })
        })
    }

    /// What [`through_faults`] took: how many times each item, and in what order.
    struct Taken {
        up: Vec<u32>,
        down: Vec<u32>,
        order_up: Vec<usize>,
        order_down: Vec<usize>,
    }

    impl Taken {
        /// Each item taken once, in its order.
        fn once_in_order(&self) -> anyhow::Result<()> {
            anyhow::ensure!(self.up.iter().all(|n| *n == 1), "updates taken: {:?}", self.up);
            anyhow::ensure!(self.down.iter().all(|n| *n == 1), "commands taken: {:?}", self.down);
            let all: Vec<usize> = (0..self.up.len()).collect();
            anyhow::ensure!(self.order_up == all && self.order_down == all, "out of order: {:?} {:?}", self.order_up, self.order_down);
            Ok(())
        }
    }

    const N: u64 = 60;

    /// Plan step 8 of "Nothing kiln sends between its processes is fire-and-forget": frames lost
    /// below the channel inside a live connection, kiln's real QUIC layer between a runner and a
    /// server. The runner drops about one in 5 of its channel frames and the server one in 4 of its,
    /// while a stream of updates goes up and a stream of commands down. The channel makes up for
    /// each (a receiver's `Want`, a sender's patience): every item is taken once and answered, in
    /// its order, and each side counted what it dropped.
    #[test]
    fn what_is_lost_below_the_channel_is_sent_again_and_taken_once() -> anyhow::Result<()> {
        use crate::proto::{LANE_CONTROL, LANE_TERM};
        let chaos = |lane: u32| crate::metrics::net().frames_dropped.get(&[crate::proto::lane_name(lane), "chaos"]);
        let (term0, control0) = (chaos(LANE_TERM), chaos(LANE_CONTROL));
        let taken = through_faults(
            "shed-1",
            &[Shed { lane: LANE_TERM, variant: Some(46), every: 5, act: Act::Drop }],
            &[Shed { lane: LANE_CONTROL, variant: Some(54), every: 4, act: Act::Drop }],
        )?;
        taken.once_in_order()?;
        anyhow::ensure!(chaos(LANE_TERM) > term0 && chaos(LANE_CONTROL) > control0, "each side counted what it dropped");
        Ok(())
    }

    /// Plan step 9 of "Nothing kiln sends between its processes is fire-and-forget": frames sent
    /// twice and out of order below the channel, beside frames lost, each side's. Every item is
    /// still taken once, in its order, and answered.
    #[test]
    fn what_comes_twice_or_late_below_the_channel_is_taken_once_in_order() -> anyhow::Result<()> {
        use crate::proto::{LANE_CONTROL, LANE_TERM};
        let faults = |lane: u32, variant: u8| {
            [
                Shed { lane, variant: Some(variant), every: 3, act: Act::Twice },
                Shed { lane, variant: Some(variant), every: 4, act: Act::Late },
                Shed { lane, variant: Some(variant), every: 7, act: Act::Drop },
            ]
        };
        through_faults("twice-1", &faults(LANE_TERM, 46), &faults(LANE_CONTROL, 54))?.once_in_order()
    }

    /// A server's owner slow to take what comes in (a hub behind its runners) goes on sending all
    /// the same: the net thread takes its sends while it holds what the owner hasn't taken, where
    /// it once waited on the owner to take one while the owner waited on it to take a send, each
    /// for good (and every connection went quiet as it lasted); then every frame that came in is
    /// the owner's, once each and each lane's in order.
    #[test]
    fn an_owner_slow_to_take_what_comes_in_goes_on_sending_and_misses_none() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let pki = crate::pki::Pki::open(dir.path())?;
            let probe = std::net::UdpSocket::bind("127.0.0.1:0")?;
            let addr = probe.local_addr()?;
            drop(probe);
            let (srv, srv_ev) = start(Spec { bind: addr, ca_der: pki.ca_der.clone(), leaf: pki.server_leaf(&[])?, server: true, dial_deadline: DIAL_DEADLINE, revoked_keys: Vec::new() })?;
            let (agent, agent_ev) = start(Spec { bind: "127.0.0.1:0".parse()?, ca_der: pki.ca_der.clone(), leaf: pki.issue_local("slow-owner")?, server: false, dial_deadline: Duration::from_secs(5), revoked_keys: Vec::new() })?;
            agent.command(Out::Connect { addr }).await;
            let deadline = Instant::now() + Duration::from_secs(60);
            let peer = connected_within(&srv_ev, Duration::from_secs(15)).await.ok_or_else(|| anyhow::anyhow!("the agent didn't connect"))?;
            loop {
                if let NetEvent::Connected { .. } = next_event(&agent_ev, deadline).await? {
                    break;
                }
            }
            // More than the owner's queue and the thread's command queue hold (16,384 each), over
            // four lanes (no lane's queue past its bound), the owner taking none of it.
            const N: u32 = 20_000;
            const LANES: u32 = 4;
            let frame = |i: u32| [(i % LANES).to_le_bytes(), i.to_le_bytes()].concat();
            for i in 0..N {
                anyhow::ensure!(agent.send(SERVER_PEER, i % LANES, Kind::Control, frame(i)).await, "the agent's thread is gone");
            }
            // Taking none still, as many go the other way (spread as well: no lane's queue past
            // its bound): no send waits for good.
            let sent = crate::rt::timeout(Duration::from_secs(30), async {
                for i in 0..N {
                    srv.send(peer, i % LANES, Kind::Control, frame(i)).await;
                }
            })
            .await;
            anyhow::ensure!(sent.is_ok(), "the server's sends waited for its owner to take what came in");
            // Then all that came in, once each, each lane's in order.
            let mut next = [0u32; LANES as usize];
            let mut taken = 0u32;
            while taken < N {
                let ev = next_event(&srv_ev, deadline).await.map_err(|e| anyhow::anyhow!("{taken} of {N} frames came in: {e}"))?;
                if let NetEvent::Frame { lane, data, .. } = ev {
                    let at = next.get_mut(lane as usize).ok_or_else(|| anyhow::anyhow!("a frame on lane {lane}"))?;
                    let i = data.get(4..8).and_then(|b| <[u8; 4]>::try_from(b).ok()).map(u32::from_le_bytes).ok_or_else(|| anyhow::anyhow!("a frame of {} bytes", data.len()))?;
                    anyhow::ensure!(i == *at * LANES + lane, "lane {lane}: {i} where {} was next", *at * LANES + lane);
                    *at += 1;
                    taken += 1;
                }
            }
            Ok(())
        })
    }

    /// Goal 3, plan 01a1a59c2a8e step 5 (perf/api-pass's leftover): an agent's dial that goes
    /// unanswered while its owner has no room for what came in waits on nothing and loses nothing.
    /// The owner takes none of what the server sends it (more than its queue holds), then asks for a
    /// dial to an address that never answers: past the dial's deadline the loop still takes commands
    /// (a sample is answered), and once the owner takes what came in, every frame is there in order,
    /// then the dial's `Unreachable`. The loop used to wait at the deadline for room to say so, taking
    /// no command meanwhile, and a rebuild dropped what it held for the owner.
    #[test]
    fn g3_s5_a_dial_unanswered_while_the_owner_has_no_room_waits_on_nothing_and_loses_nothing() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let pki = crate::pki::Pki::open(dir.path())?;
            let probe = std::net::UdpSocket::bind("127.0.0.1:0")?;
            let addr = probe.local_addr()?;
            drop(probe);
            let (srv, srv_ev) = start(Spec { bind: addr, ca_der: pki.ca_der.clone(), leaf: pki.server_leaf(&[])?, server: true, dial_deadline: DIAL_DEADLINE, revoked_keys: Vec::new() })?;
            // The owner's queue bounded at a mebibyte (goal 11, plan 01a1a87b2e56 step 3: in bytes, not
            // events), so what's sent below fills it and leaves the net thread more to hold.
            const ROOM: usize = 1 << 20;
            let (agent, agent_ev) = start_bounded(Spec { bind: "127.0.0.1:0".parse()?, ca_der: pki.ca_der.clone(), leaf: pki.issue_local("full-owner")?, server: false, dial_deadline: Duration::from_secs(1), revoked_keys: Vec::new() }, OUT_BYTES, ROOM)?;
            // Full: no room for one more of the frames below (each eight bytes, counted with its place).
            let one = weight(&NetEvent::Frame { peer: SERVER_PEER, lane: 0, kind: Kind::Control, data: vec![0; 8], at: Instant::now() }).saturating_add(crate::memory::queue::PLACE);
            let full = |rx: &crate::memory::queue::Receiver<NetEvent>| rx.held().saturating_add(one) > rx.bound();
            agent.command(Out::Connect { addr }).await;
            let deadline = Instant::now() + Duration::from_secs(60);
            let peer = connected_within(&srv_ev, Duration::from_secs(15)).await.ok_or_else(|| anyhow::anyhow!("the agent didn't connect"))?;
            loop {
                if let NetEvent::Connected { .. } = next_event(&agent_ev, deadline).await? {
                    break;
                }
            }
            // More than the owner's queue holds (about 10,000 of these), the owner taking none of it, over
            // four lanes (no lane's queue past its bound).
            const N: u32 = 17_000;
            const LANES: u32 = 4;
            for i in 0..N {
                srv.send(peer, i % LANES, Kind::Control, [(i % LANES).to_le_bytes(), i.to_le_bytes()].concat()).await;
            }
            let filled = Instant::now() + Duration::from_secs(30);
            while !full(&agent_ev) && Instant::now() < filled {
                crate::rt::sleep(Duration::from_millis(10)).await;
            }
            anyhow::ensure!(full(&agent_ev), "the owner's queue never filled: {} of {} bytes", agent_ev.held(), agent_ev.bound());
            let queued = u32::try_from(agent_ev.len()).unwrap_or(u32::MAX);
            // A dial to an address that takes the handshake and never answers, past its deadline.
            let silent = std::net::UdpSocket::bind("127.0.0.1:0")?;
            agent.command(Out::Connect { addr: silent.local_addr()? }).await;
            crate::rt::sleep(Duration::from_millis(2500)).await;
            let asking = agent.clone();
            let sampled = crate::rt::on_thread("sample", move || asking.sample(Duration::from_secs(10))).await?;
            anyhow::ensure!(sampled.is_some(), "the net thread took no command past the dial's deadline: it waited for the owner");
            // Then what came in, each lane's frames in order, and after them the dial's Unreachable.
            let mut next = [0u32; LANES as usize];
            loop {
                match next_event(&agent_ev, deadline).await? {
                    NetEvent::Frame { lane, data, .. } => {
                        let at = next.get_mut(lane as usize).ok_or_else(|| anyhow::anyhow!("a frame on lane {lane}"))?;
                        let i = data.get(4..8).and_then(|b| <[u8; 4]>::try_from(b).ok()).map(u32::from_le_bytes).ok_or_else(|| anyhow::anyhow!("a frame of {} bytes", data.len()))?;
                        anyhow::ensure!(i == *at * LANES + lane, "lane {lane}: {i} where {} was next", *at * LANES + lane);
                        *at += 1;
                    }
                    NetEvent::Unreachable { peer, reason } if peer == SERVER_PEER => {
                        let taken: u32 = next.iter().sum();
                        anyhow::ensure!(taken >= queued, "the Unreachable came after {taken} frames, fewer than the owner's queue held ({queued}): {reason}");
                        break;
                    }
                    _ => {}
                }
            }
            drop(silent);
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
            let (srv, srv_ev) = start(Spec { bind: addr, ca_der: pki.ca_der.clone(), leaf: server_leaf, server: true, dial_deadline: DIAL_DEADLINE, revoked_keys: Vec::new() })?;
            let agent_leaf = pki.issue_local("bench-1")?;
            let (agent, agent_ev) = start(Spec {
                bind: "127.0.0.1:0".parse()?,
                ca_der: pki.ca_der.clone(),
                leaf: agent_leaf,
                server: false,
                dial_deadline: Duration::from_secs(2),
                revoked_keys: Vec::new(),
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

