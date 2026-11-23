//! Direct runner-to-runner forwarding over `hyper-quic` (TLS 1.3 mutual authentication pinned by
//! each runner's reach identity fingerprint, AES-256-GCM + ML-KEM hybrid post-quantum key exchange,
//! multiplexed bidirectional QUIC streams on `hyper_rt::udp::Batched`).
//!
//! Strictly separates forwarding from Raft replication:
//! - `KIND_HARNESS_SEND` (1): forwards a user/agent message directly to the session's owning runner
//!   over a dedicated bidirectional QUIC stream and returns the owning inbox's [`wire::P2pSendReply`]
//!   on the same stream — zero central server hop, zero WebSocket framing, zero Raft hijacking.
//! - `KIND_CONVO_RAFT` (2): carries per-session conversation Raft messages ([`crate::convo::raft`])
//!   directly between peers over QUIC when UDP reaches them (falling back transparently to relayed
//!   QUIC on `LANE_REACH` when NAT prevents direct UDP).

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::{Future as _, poll_fn};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use bytes::BytesMut;
use futures::channel::oneshot;
use hyper_quic::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use hyper_quic::rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use hyper_quic::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use hyper_quic::rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use hyper_quic::rustls::{self, DigitallySignedStruct, DistinguishedName, SignatureScheme};
use hyper_quic::{
    ClientConfig, ClientConfigHandle, Connection, ConnectionHandle, DatagramEvent, Dir, Endpoint, EndpointConfig,
    Event as QuicEvent, ReadError, ServerConfig, StreamEvent, StreamId, TransportConfig, VarInt, WriteError,
};
use hyper_rt::futures::{Sleep, sleep};
use hyper_rt::readiness::Ready;
use hyper_rt::udp::{Batched, Io, Sent};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::e2e::{self, Identity};
use super::wire::{self, P2pSendReply};

/// ALPN for direct runner-to-runner `hyper-quic` forwarding connections.
const ALPN: &[u8] = b"kiln-p2p-quic/1";
/// Stream tag: direct peer-to-peer routed harness message (request + reply on one bidi stream).
const KIND_HARNESS_SEND: u8 = 1;
/// Stream tag: per-session conversation Raft frame (one-way framed message).
const KIND_CONVO_RAFT: u8 = 2;
/// Maximum payload carried on one direct QUIC stream (matches reach wire limit).
const MAX_STREAM_PAYLOAD: usize = wire::MAX_MESSAGE;
const CONNECTION_WINDOW: u32 = 16 << 20;
/// Datagram batch per syscall.
const BATCH: usize = 32;
/// Max turns per poll before yielding to the shard.
const TURNS: usize = 64;
/// Max concurrent bidirectional streams per peer QUIC connection.
const MAX_BIDI_STREAMS: u32 = 128;
/// Max concurrent unidirectional streams per peer QUIC connection.
const MAX_UNI_STREAMS: u32 = 64;
/// A cold path has no measured RTT; use the transport's initial estimate and allow QUIC to retry.
pub(super) const DIAL_TIMEOUT: Duration = Duration::from_millis(crate::harness::told::Rto::INITIAL_MS).saturating_mul(4);
/// QUIC NAT keep-alive interval for established direct peer connections.
const KEEP_ALIVE: Duration = Duration::from_secs(15);
/// Idle timeout for direct peer QUIC connections (six keep-alive intervals).
const IDLE_TIMEOUT: Duration = KEEP_ALIVE.saturating_mul(6);
/// An established RPC may await a durable destination receipt for its entire connection idle window.
pub(crate) const REQUEST_WAIT: Duration = IDLE_TIMEOUT;
/// Maximum queued outbound frames per peer: matches one full stream-window wave (bidi + uni).
const MAX_PENDING_OUT: usize = (MAX_BIDI_STREAMS as usize) + (MAX_UNI_STREAMS as usize);

/// An inbound routed harness message received over direct `hyper-quic` (or relayed reach link).
pub struct HarnessSendAsk {
    pub from: String,
    pub session: String,
    pub epoch: Option<u64>,
    pub json: String,
    pub reply: oneshot::Sender<P2pSendReply>,
    pub(super) _room: Option<ReadRoom>,
}

/// An inbound per-session conversation Raft frame received from a peer node.
pub struct ConvoRaftIn {
    pub from: String,
    pub session: String,
    pub bytes: Vec<u8>,
    pub(super) _room: Option<ReadRoom>,
}

/// Peer addressing metadata learned from the reach directory & TOFU pins.
#[derive(Debug, Clone)]
pub struct PeerEntry {
    pub name: String,
    pub fingerprint: String,
    pub addrs: Vec<SocketAddr>,
}

#[derive(Debug, Serialize, Deserialize)]
struct DirectHarnessSend {
    session: String,
    epoch: Option<u64>,
    json: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct DirectConvoRaft {
    session: String,
    from: String,
    bytes: Vec<u8>,
}

/// Encode a 64-hex SHA-256 SPKI fingerprint into a valid DNS SNI label pair (`<32>.<32>.kiln-p2p`).
fn fp_server_name(fp: &str) -> Option<String> {
    if fp.len() != 64 || !fp.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let a = fp.get(..32)?;
    let b = fp.get(32..64)?;
    Some(format!("{a}.{b}.kiln-p2p"))
}

/// Decode `<32>.<32>.kiln-p2p` back into the expected 64-hex fingerprint.
fn fp_from_server_name(name: &ServerName<'_>) -> Option<String> {
    let ServerName::DnsName(dns) = name else { return None };
    let raw = dns.as_ref().strip_suffix(".kiln-p2p")?;
    let (a, b) = raw.split_once('.')?;
    if a.len() == 32 && b.len() == 32 {
        Some(format!("{a}{b}").to_ascii_lowercase())
    } else {
        None
    }
}

/// Server-side TLS 1.3 client verifier: admits any peer whose reach identity SPKI fingerprint is
/// registered in our directory/pin table.
#[derive(Debug)]
struct KnownPeersVerifier {
    by_fp: Arc<Mutex<HashMap<String, String>>>,
}

impl ClientCertVerifier for KnownPeersVerifier {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(&self, end_entity: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: UnixTime) -> Result<ClientCertVerified, rustls::Error> {
        let fp = e2e::fingerprint_of_cert(end_entity).ok_or(rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding))?;
        if self.by_fp.lock().contains_key(&fp) {
            Ok(ClientCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(rustls::CertificateError::ApplicationVerificationFailure))
        }
    }

    fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(rustls::PeerIncompatible::Tls13RequiredForQuic))
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &e2e::provider().signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        e2e::provider().signature_verification_algorithms.supported_schemes()
    }
}

/// Client-side TLS 1.3 server verifier: verifies the dialed peer presents the exact pinned SPKI
/// fingerprint encoded in `server_name` (`<32>.<32>.kiln-p2p`).
#[derive(Debug)]
struct PinnedServerVerifier;

impl ServerCertVerifier for PinnedServerVerifier {
    fn verify_server_cert(&self, end_entity: &CertificateDer<'_>, _: &[CertificateDer<'_>], server_name: &ServerName<'_>, _: &[u8], _: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        let want = fp_from_server_name(server_name).ok_or(rustls::Error::InvalidCertificate(rustls::CertificateError::ApplicationVerificationFailure))?;
        let got = e2e::fingerprint_of_cert(end_entity).ok_or(rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding))?;
        if got.eq_ignore_ascii_case(&want) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(rustls::CertificateError::ApplicationVerificationFailure))
        }
    }

    fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(rustls::PeerIncompatible::Tls13RequiredForQuic))
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &e2e::provider().signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        e2e::provider().signature_verification_algorithms.supported_schemes()
    }
}

fn transport_config() -> TransportConfig {
    let mut t = TransportConfig::default();
    t.max_concurrent_bidi_streams(VarInt::from_u32(MAX_BIDI_STREAMS))
        .max_concurrent_uni_streams(VarInt::from_u32(MAX_UNI_STREAMS))
        .receive_window(VarInt::from_u32(CONNECTION_WINDOW))
        .stream_receive_window(VarInt::from_u32(4 << 20))
        .send_window(u64::from(CONNECTION_WINDOW))
        .max_idle_timeout(hyper_quic::IdleTimeout::try_from(IDLE_TIMEOUT).ok())
        .keep_alive_interval(Some(KEEP_ALIVE));
    t
}

fn build_server_config(identity: &Identity, by_fp: Arc<Mutex<HashMap<String, String>>>) -> anyhow::Result<ServerConfig> {
    let verifier = Box::new(KnownPeersVerifier { by_fp });
    let mut tls = rustls::ServerConfig::builder_with_provider(e2e::provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .context("p2p QUIC server TLS versions")?
        .with_client_cert_verifier(verifier)
        .with_single_cert(identity.chain(), identity.key())
        .context("p2p QUIC server identity")?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    tls.max_early_data_size = 0;
    tls.send_tls13_tickets = 0;
    let crypto = QuicServerConfig::try_from(tls).map_err(|e| anyhow::anyhow!("p2p QUIC server crypto: {e:?}"))?;
    let mut cfg = ServerConfig::with_crypto(Box::new(crypto));
    cfg.transport_config(transport_config());
    cfg.migration(true);
    Ok(cfg)
}

fn build_client_config(identity: &Identity) -> anyhow::Result<ClientConfig> {
    let mut tls = rustls::ClientConfig::builder_with_provider(e2e::provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .context("p2p QUIC client TLS versions")?
        .dangerous()
        .with_custom_certificate_verifier(Box::new(PinnedServerVerifier))
        .with_client_auth_cert(identity.chain(), identity.key())
        .context("p2p QUIC client identity")?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    tls.resumption = rustls::client::Resumption::disabled();
    tls.enable_early_data = false;
    let crypto = QuicClientConfig::try_from(tls).map_err(|e| anyhow::anyhow!("p2p QUIC client crypto: {e:?}"))?;
    let mut cfg = ClientConfig::new(Box::new(crypto));
    cfg.transport_config(transport_config());
    Ok(cfg)
}

/// Discover advertised UDP `ip:port` endpoints for a direct `hyper-quic` listener bound to `local`.
pub fn advertised_addrs(local: SocketAddr) -> Vec<String> {
    let port = local.port();
    if port == 0 {
        return Vec::new();
    }
    let mut addrs = Vec::new();
    if local.ip().is_unspecified() {
        for iface in crate::discovery::interfaces(&[]) {
            for (v4, _) in iface.v4 {
                if !v4.is_loopback() && !v4.is_unspecified() {
                    let s = SocketAddr::new(IpAddr::V4(v4), port).to_string();
                    if !addrs.contains(&s) {
                        addrs.push(s);
                    }
                }
            }
        }
        if addrs.is_empty() {
            addrs.push(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port).to_string());
        }
    } else {
        addrs.push(local.to_string());
    }
    addrs
}

/// Commands into the direct `hyper-quic` driver task.
enum Cmd {
    UpdatePeers(Vec<PeerEntry>),
    WarmPeer(String),
    ReapCanceled(String),
    SendHarness {
        peer: String,
        payload: OutFrame,
        reply: oneshot::Sender<anyhow::Result<P2pSendReply>>,
    },
    SendConvoRaft {
        peer: String,
        payload: OutFrame,
    },
    StreamReply {
        conn_id: usize,
        incarnation: Arc<()>,
        stream_id: StreamId,
        frame: Option<ReplyFrame>,
    },
}

/// Handle to this runner's direct `hyper-quic` forwarding endpoint (pointer-sized `Arc` handle).
#[derive(Clone)]
pub struct PeerQuic(Arc<PeerQuicInner>);

/// The reply receiver is dropped first, so even cancellation by the caller can wake the driver
/// to withdraw work still waiting for stream credit.
struct CancelRequest { quic: PeerQuic, peer: String, reply: Option<oneshot::Receiver<anyhow::Result<P2pSendReply>>> }
impl CancelRequest {
    async fn wait(mut self, since: Instant) -> anyhow::Result<P2pSendReply> {
        let rx = self.reply.as_mut().context("reply already taken")?;
        let result = crate::rt::timeout(REQUEST_WAIT.saturating_sub(since.elapsed()), rx).await?
            .map_err(|_| anyhow::anyhow!("direct QUIC request to {} dropped", self.peer))?;
        self.reply.take();
        result
    }
}
impl Drop for CancelRequest {
    fn drop(&mut self) {
        if self.reply.take().is_some() {
            let _ = self.quic.0.tx.try_send(Cmd::ReapCanceled(self.peer.to_owned()));
            self.quic.0.wake.notify();
        }
    }
}

impl Drop for PeerQuicInner {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        self.wake.notify();
    }
}

struct PeerQuicInner {
    detector: Option<(std::sync::Weak<crate::liveness::Shared>, u64)>,
    start_liveness:std::sync::OnceLock<crate::liveness::Subscription>,
    start_waits:Mutex<HashMap<u64,std::sync::Weak<crate::rt::Changed>>>,
    stopping: Arc<AtomicBool>,
    tx: crate::memory::queue::Sender<Cmd>,
    wake: Arc<crate::rt::Notify>,
    local_addr: SocketAddr,
    advertised: Vec<String>,
    known_peers: Arc<Mutex<HashMap<String, Vec<SocketAddr>>>>,
    connected_peers: Arc<Mutex<HashSet<String>>>,
    starts: Arc<std::sync::OnceLock<std::sync::Weak<crate::frontdoor::start_outbox::Outbox>>>,
    failed_until: Arc<Mutex<HashMap<String, Instant>>>,
    frames: Arc<FrameRoom>,
}

impl PeerQuic {
    /// Start a direct `hyper-quic` listener on `bind` (`0.0.0.0:0` or `127.0.0.1:0` in tests).
    pub fn start(
        bind: SocketAddr,
        identity: &Identity,
        harness_send: Option<crate::memory::queue::Sender<HarnessSendAsk>>,
        convo_raft: Option<crate::memory::queue::Sender<ConvoRaftIn>>,
    ) -> anyhow::Result<Self> {
        Self::start_on_node(bind, identity, harness_send, convo_raft, None)
    }

    pub(super) fn start_on_node(
        bind: SocketAddr, identity: &Identity,
        harness_send: Option<crate::memory::queue::Sender<HarnessSendAsk>>,
        convo_raft: Option<crate::memory::queue::Sender<ConvoRaftIn>>,
        state_dir: Option<&std::path::Path>,
    ) -> anyhow::Result<Self> {
        let by_fp: Arc<Mutex<HashMap<String, String>>> = Arc::default();
        let server_cfg = build_server_config(identity, Arc::clone(&by_fp))?;
        let client_cfg = build_client_config(identity)?;
        let mut endpoint = Endpoint::new(EndpointConfig::default(), Some(server_cfg), true, None)
            .map_err(|e| anyhow::anyhow!("create p2p hyper-quic endpoint: {e:?}"))?;
        let client_handle = endpoint.insert_client_config(client_cfg).map_err(|e| anyhow::anyhow!("insert p2p client config: {e:?}"))?;
        let socket = Batched::bind(bind, Io { batch: BATCH }, state_dir.is_some()).map_err(|e| anyhow::anyhow!("bind p2p hyper-quic socket {bind}: {e}"))?;
        let local_addr = socket.local_addr().map_err(|e| anyhow::anyhow!("p2p hyper-quic local_addr: {e}"))?;
        let advertised = advertised_addrs(local_addr);
        let (tx, rx) = crate::memory::queue::unbounded::<Cmd>(&crate::memory::owners::CHANNELS);
        let wake = Arc::new(crate::rt::Notify::new());
        let known_peers: Arc<Mutex<HashMap<String, Vec<SocketAddr>>>> = Arc::default();
        let connected_peers: Arc<Mutex<HashSet<String>>> = Arc::default();
        let starts = Arc::new(std::sync::OnceLock::new());
        let failed_until: Arc<Mutex<HashMap<String, Instant>>> = Arc::default();
        let frames = Arc::new(FrameRoom::new());
        let incoming = Arc::new(crate::memory::queue::Room::new(&crate::memory::owners::CHANNELS, frame_budget()));

        let stopping = Arc::new(AtomicBool::new(false));
        let liveness = state_dir.filter(|_| socket.stats().kernel_stamps)
            .map(|dir| super::heartbeat::Plane::open(dir, identity, &wake)).transpose()?;
        let detector = liveness.as_ref().map(super::heartbeat::Plane::incarnation);
        let state = DriverState {
            liveness,
            stopping: Arc::clone(&stopping),
            endpoint,
            client_handle,
            socket,
            by_fp,
            peers: HashMap::new(),
            addr_cursor: HashMap::new(),
            dial_time: HashMap::new(),
            connected_peers: Arc::clone(&connected_peers),
            starts: Arc::clone(&starts),
            failed_until: Arc::clone(&failed_until),
            incoming,
            conns: Vec::new(),
            by_peer: HashMap::new(),
            dirty: VecDeque::new(),
            out_buf: Vec::with_capacity(1500),
            arrived: Vec::new(),
            replies: Vec::new(),
            readable: None,
            writable: None,
            timer: None,
            cmd_rx: rx,
            cmd_tx: tx.clone(),
            wake: Arc::clone(&wake),
            harness_send,
            convo_raft,
        };
        crate::rt::spawn(async move {
            let mut state = state;
            if let Err(error) = state.run().await {tracing::warn!(%error, "reach datagram driver stopped");}
        });

        Ok(Self(Arc::new(PeerQuicInner { detector, start_liveness:Default::default(),start_waits:Default::default(),stopping, tx, wake, local_addr, advertised, known_peers, connected_peers, starts, failed_until, frames })))
    }

    pub(crate) fn has_liveness(&self) -> bool {
        self.0.detector.as_ref().is_some_and(|(service, generation)| service.upgrade()
            .is_some_and(|s| s.with_generation(*generation, |_| ()).is_some()))
    }

    pub(crate) fn watch_starts(&self, outbox: &Arc<crate::frontdoor::start_outbox::Outbox>) {
        if self.0.starts.set(Arc::downgrade(outbox)).is_err() {
            return;
        }
        let Some((shared, _)) = &self.0.detector else {
            return;
        };
        let Some(shared) = shared.upgrade() else {
            return;
        };
        let endpoint = Arc::downgrade(&self.0);
        let watch = shared.subscribe(Arc::new(move |event| {
            let Some(endpoint) = endpoint.upgrade() else {
                return;
            };
            let crate::liveness::Event::Change(change) = event else {
                return;
            };
            let peer = match change {
                hyper_liveness::Change::Trusted { peer, .. } | hyper_liveness::Change::Restarted { peer, .. } => peer,
                hyper_liveness::Change::Suspected(_) | hyper_liveness::Change::Stopped { .. } => {
                    if let Some(signal) = endpoint.start_waits.lock().remove(&change.peer()).and_then(|weak| weak.upgrade()) {
                        signal.notify();
                    }
                    return;
                }
                _ => return,
            };
            let name = endpoint.known_peers.lock().keys().find(|name| crate::convo::raft::node_id(name) == peer).cloned();
            if let Some(name) = name
                && let Some(outbox) = endpoint.starts.get().and_then(std::sync::Weak::upgrade)
            {
                outbox.linked(&name);
            }
        }));
        let _ = self.0.start_liveness.set(watch);
    }

    /// Stop waiting for an established dead connection when the shared node-pair detector
    /// has evidence for this peer. A live runner may take longer to commit a founding record.
    pub(crate) async fn forward_start(&self, peer: &str, body: String) -> anyhow::Result<P2pSendReply> {
        let Some((shared, generation)) = &self.0.detector else {
            return self.begin_harness_message(peer, "", None, body)?.await;
        };
        let Some(shared) = shared.upgrade() else {
            return self.begin_harness_message(peer, "", None, body)?.await;
        };
        let id = crate::convo::raft::node_id(peer);
        let signal = {
            let mut peers = self.0.start_waits.lock();
            if let Some(active) = peers.get(&id).and_then(std::sync::Weak::upgrade) {
                active
            } else {
                let changed = Arc::new(crate::rt::Changed::new());
                peers.insert(id, Arc::downgrade(&changed));
                changed
            }
        };
        let wait = signal.listen();
        if shared.with_generation(*generation, |l| l.stopped(id) || l.trust(id) == Some(hyper_timing::Trust::Suspected)) == Some(true) {
            anyhow::bail!("the selected runner is unreachable");
        }
        match futures::future::select(self.begin_harness_message(peer, "", None, body)?, wait).await {
            futures::future::Either::Left((answer, _)) => answer,
            futures::future::Either::Right(_) => anyhow::bail!("the selected runner's link went down before its receipt"),
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.0.local_addr
    }

    pub fn advertised_addrs(&self) -> &[String] {
        &self.0.advertised
    }

    /// Update the peer directory (fingerprint + direct UDP `quic_addrs`).
    pub fn update_peers(&self, mut peers: Vec<PeerEntry>) {
        let mut by_name = HashMap::new();
        let mut by_key = HashMap::new();
        let mut bad_names = HashSet::new();
        let mut bad_keys = HashSet::new();
        for entry in &mut peers { entry.fingerprint.make_ascii_lowercase(); }
        for entry in &peers {
            if by_name.insert(&entry.name, &entry.fingerprint).is_some_and(|previous| previous != &entry.fingerprint) { bad_names.insert(entry.name.clone()); }
            if by_key.insert(&entry.fingerprint, &entry.name).is_some_and(|previous| previous != &entry.name) { bad_keys.insert(entry.fingerprint.clone()); }
        }
        peers.retain(|p| !bad_names.contains(&p.name) && !bad_keys.contains(&p.fingerprint));
        {
            let mut k = self.0.known_peers.lock();
            k.clear();
            for p in &peers {
                if !p.addrs.is_empty() {
                    k.insert(p.name.clone(), p.addrs.clone());
                }
            }
        }
        let _ = self.0.tx.try_send(Cmd::UpdatePeers(peers));
        self.0.wake.notify();
    }

    /// Whether `peer` currently advertises at least one direct `hyper-quic` UDP endpoint and is not
    /// in post-timeout cooldown (e.g. NAT/firewall blocking direct UDP).
    pub(crate) fn knows_peer(&self, peer: &str) -> bool {
        self.0.known_peers.lock().contains_key(peer)
    }

    pub fn has_direct(&self, peer: &str) -> bool {
        if self.0.failed_until.lock().get(peer).is_some_and(|until| Instant::now() < *until) {
            return false;
        }
        self.0.known_peers.lock().get(peer).is_some_and(|a| !a.is_empty())
    }

    /// Whether a direct `hyper-quic` connection to `peer` is already handshaked and ready right now
    /// (zero dial latency).
    pub fn is_connected(&self, peer: &str) -> bool {
        self.0.connected_peers.lock().contains(peer)
    }

    /// Initiate a non-blocking background direct `hyper-quic` handshake to `peer` if eligible so
    /// subsequent traffic can route over QUIC without stalling a live WebSocket stream.
    pub fn warm_peer(&self, peer: &str) {
        if self.has_direct(peer) && !self.is_connected(peer) {
            let _ = self.0.tx.try_send(Cmd::WarmPeer(peer.to_owned()));
            self.0.wake.notify();
        }
    }

    /// Forward a routed harness message directly to `peer` over a bidirectional `hyper-quic` stream.
    pub(crate) fn begin_harness_message(&self, peer: &str, session: &str, epoch: Option<u64>, json: String) -> anyhow::Result<futures::future::BoxFuture<'static, anyhow::Result<P2pSendReply>>> {
        let payload = self.0.frames.encode(peer, KIND_HARNESS_SEND, &(session, epoch, json.as_str()))?;
        let (reply_tx, reply_rx) = oneshot::channel();
        self.0.tx.try_send(Cmd::SendHarness { peer: peer.to_owned(), payload, reply: reply_tx })
            .map_err(|_| anyhow::anyhow!("direct QUIC driver stopped"))?;
        self.0.wake.notify();
        let cancel = CancelRequest { quic: self.clone(), peer: peer.to_owned(), reply: Some(reply_rx) };
        Ok(Box::pin(cancel.wait(Instant::now())))
    }

    #[cfg(test)]
    async fn send_harness_message(&self, peer: &str, session: &str, epoch: Option<u64>, json: String) -> anyhow::Result<P2pSendReply> {
        self.begin_harness_message(peer, session, epoch, json)?.await
    }

    /// Send a per-session conversation Raft message directly to `peer` over `hyper-quic`.
    /// Returns `false` if `peer` has no direct QUIC address, is in cooldown, or its outbound QUIC
    /// pending queue is saturated (so callers can fall back or report snapshot failure accurately).
    pub fn send_convo_raft(&self, peer: &str, session: &str, from: &str, bytes: &[u8]) -> bool {
        if !self.has_direct(peer) {
            return false;
        }
        let Ok(payload) = self.0.frames.encode(peer, KIND_CONVO_RAFT, &(session, from, Octets(bytes))) else { return false };
        let sent = self.0.tx.try_send(Cmd::SendConvoRaft { peer: peer.to_owned(), payload }).is_ok();
        if sent {
            self.0.wake.notify();
        }
        sent
    }
}

// Postcard encodes a byte string identically to Vec<u8>. Borrowing it keeps Raft's original
// available for relay without a deep clone, and permits size calculation in constant time.
struct Octets<'a>(&'a [u8]);
impl Serialize for Octets<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> { s.serialize_bytes(self.0) }
}

pub(super) fn frame_budget() -> usize {
    crate::memory::room::share().clamp(MAX_STREAM_PAYLOAD + 5, CONNECTION_WINDOW as usize)
}
pub(super) struct FrameRoom {
    bytes: crate::memory::queue::Room,
    counts: Mutex<HashMap<String, usize>>,
}
impl FrameRoom {
    pub(super) fn new() -> Self {
        Self { bytes: crate::memory::queue::Room::new(&crate::memory::owners::CHANNELS, frame_budget()), counts: Mutex::default() }
    }

    pub(super) fn reserve(self: &Arc<Self>, peer: &str, bytes: usize) -> anyhow::Result<FrameCharge> {
        {
            let mut counts = self.counts.lock();
            anyhow::ensure!(counts.get(peer).copied().unwrap_or_default() < MAX_PENDING_OUT && self.bytes.try_take(bytes), "direct QUIC send budget is full");
            *counts.entry(peer.to_owned()).or_default() += 1;
        }
        Ok(FrameCharge { room: Arc::clone(self), peer: peer.to_owned(), bytes })
    }

    fn encode<T: Serialize>(self: &Arc<Self>, peer: &str, kind: u8, body: &T) -> anyhow::Result<OutFrame> {
        let size = postcard::experimental::serialized_size(body)?;
        anyhow::ensure!(size <= MAX_STREAM_PAYLOAD, "direct QUIC payload exceeds limit");
        let bytes = size.saturating_add(5);
        let charge = self.reserve(peer, bytes)?;
        let mut out = Vec::with_capacity(bytes);
        out.push(kind);
        out.extend_from_slice(&u32::try_from(size)?.to_be_bytes());
        Ok(OutFrame { data: postcard::to_extend(body, out)?, charge })
    }
}
struct OutFrame { data: Vec<u8>, charge: FrameCharge }
pub struct FrameCharge { room: Arc<FrameRoom>, peer: String, bytes: usize }
impl FrameCharge {
    fn wrote(&mut self) { self.room.bytes.give(std::mem::take(&mut self.bytes)); }
}
impl Drop for FrameCharge {
    fn drop(&mut self) {
        if self.bytes != 0 { self.wrote(); }
        let mut counts = self.room.counts.lock();
        if let Some(n) = counts.get_mut(&self.peer) {
            *n = n.saturating_sub(1);
            if *n == 0 { counts.remove(&self.peer); }
        }
    }
}
pub(super) struct ReadRoom { room: Arc<crate::memory::queue::Room>, bytes: usize }
impl ReadRoom {
    pub(super) fn take(room: &Arc<crate::memory::queue::Room>, bytes: usize) -> Option<Self> {
        room.try_take(bytes).then(|| Self { room: room.clone(), bytes })
    }
}
impl Drop for ReadRoom {
    fn drop(&mut self) { if self.bytes != 0 { self.room.give(self.bytes); } }
}

struct ReplyFrame { data: Vec<u8>, charge: ReadRoom }
impl std::io::Write for ReplyFrame {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let needed = self.data.len().saturating_add(bytes.len());
        if needed > MAX_STREAM_PAYLOAD + 4 { return Err(std::io::ErrorKind::FileTooLarge.into()); }
        if needed > self.charge.bytes {
            let to = needed.next_power_of_two().min(MAX_STREAM_PAYLOAD + 4);
            if !self.charge.room.try_take(to - self.charge.bytes) { return Err(std::io::ErrorKind::WouldBlock.into()); }
            self.charge.bytes = to;
            self.data.try_reserve_exact(to - self.data.len()).map_err(std::io::Error::other)?;
        }
        self.data.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}
fn frame_reply(reply: &P2pSendReply, room: Arc<crate::memory::queue::Room>) -> Option<ReplyFrame> {
    use std::io::Write as _;
    let mut frame = ReplyFrame { data: Vec::new(), charge: ReadRoom {room,bytes:0} };
    frame.write_all(&[0; 4]).ok()?;
    if serde_json::to_writer(&mut frame, reply).is_err() {
        frame.data.truncate(4);
        frame.write_all(br#"{"t":"error","why":"reply exceeds frame budget"}"#).ok()?;
    }
    let len = u32::try_from(frame.data.len().saturating_sub(4)).ok()?;
    *frame.data.first_chunk_mut::<4>()? = len.to_be_bytes();
    Some(frame)
}

fn write_reply(quic: &mut Connection, stream: StreamId, sb: &mut StreamBuf, frame: Option<ReplyFrame>) -> bool {
    let Some(frame) = frame else {
        let _ = quic.send_stream(stream).reset(VarInt::from_u32(0));
        return false;
    };
    sb.write = frame.data;
    sb.read_room = frame.charge;
    sb.write_pos = 0;
    sb.finish_write = true;
    flush_stream_write(quic, stream, sb)
}

struct PendingOut {
    payload: OutFrame,
    reply: Option<oneshot::Sender<anyhow::Result<P2pSendReply>>>,
}

struct StreamBuf {
    send_charge: Option<FrameCharge>,
    read_room: ReadRoom,
    write: Vec<u8>,
    write_pos: usize,
    finish_write: bool,
    read: Vec<u8>,
    reply: Option<oneshot::Sender<anyhow::Result<P2pSendReply>>>,
    inbound: bool,
    dispatched: bool,
}

struct PeerConn {
    incoming: Arc<crate::memory::queue::Room>,
    incarnation: Arc<()>,
    fingerprint: Option<String>,
    handle: ConnectionHandle,
    quic: Connection,
    peer_name: Option<String>,
    connected: bool,
    dial_timed_out: bool,
    started_at: Instant,
    dial_due: Instant,
    dirty: bool,
    pending_out: VecDeque<PendingOut>,
    streams: HashMap<StreamId, StreamBuf>,
}

struct DriverState {
    liveness: Option<super::heartbeat::Plane>,
    stopping: Arc<AtomicBool>,
    endpoint: Endpoint,
    client_handle: ClientConfigHandle,
    socket: Batched,
    by_fp: Arc<Mutex<HashMap<String, String>>>,
    peers: HashMap<String, PeerEntry>,
    addr_cursor: HashMap<String, usize>,
    dial_time: HashMap<String, crate::harness::told::Rto>,
    connected_peers: Arc<Mutex<HashSet<String>>>,
    starts: Arc<std::sync::OnceLock<std::sync::Weak<crate::frontdoor::start_outbox::Outbox>>>,
    failed_until: Arc<Mutex<HashMap<String, Instant>>>,
    incoming: Arc<crate::memory::queue::Room>,
    conns: Vec<Option<PeerConn>>,
    by_peer: HashMap<String, usize>,
    dirty: VecDeque<usize>,
    out_buf: Vec<u8>,
    arrived: Vec<(SocketAddr, BytesMut)>,
    replies: Vec<(SocketAddr, Vec<u8>)>,
    readable: Option<Ready>,
    writable: Option<Ready>,
    timer: Option<(Instant, Sleep)>,
    cmd_rx: crate::memory::queue::Receiver<Cmd>,
    cmd_tx: crate::memory::queue::Sender<Cmd>,
    wake: Arc<crate::rt::Notify>,
    harness_send: Option<crate::memory::queue::Sender<HarnessSendAsk>>,
    convo_raft: Option<crate::memory::queue::Sender<ConvoRaftIn>>,
}

impl DriverState {
    async fn run(&mut self) -> Result<(), hyper_rt::RtError> {
        let mut wake_fut: Option<Pin<Box<dyn Future<Output = ()> + Send + '_>>> = None;
        // Drive via a dedicated poll loop waking on UDP socket readiness, QUIC timers, and app commands.
        let wake = Arc::clone(&self.wake);
        poll_fn(|cx| self.poll_step(cx, &wake, &mut wake_fut)).await
    }

    fn poll_step(&mut self, cx: &mut Context<'_>, wake: &Arc<crate::rt::Notify>, wake_fut: &mut Option<Pin<Box<dyn Future<Output = ()> + Send + '_>>>) -> Poll<Result<(), hyper_rt::RtError>> {
        for _ in 0..TURNS {
            if self.stopping.load(Ordering::Acquire) {
                if let Some(plane)=&mut self.liveness { plane.drive(hyper_rt::machine::clock::shard_clock_ns(),true); plane.send(&mut self.socket); self.socket.send(); }
                return Poll::Ready(Ok(()));
            }
            let now = Instant::now();
            // 1. Process incoming commands from local callers and inbound stream reply tasks.
            let mut moved = false;
            for _ in 0..MAX_PENDING_OUT {
                let Ok(cmd) = self.cmd_rx.try_recv() else { break };
                moved = true;
                self.on_cmd(cmd, now);
            }

            // 2. Receive UDP datagrams from the socket.
            // AD-1. Judge at a time sampled before draining UDP. A scheduling pause after
            // draining cannot turn an already-arrived heartbeat into apparent silence.
            let epoch = hyper_rt::registry::with_current(|ctx| ctx.host_epoch_ns()).flatten();
            let judge_at = hyper_rt::machine::clock::shard_clock_ns();
            let mut through = None;
            let mut got = 0usize;
            loop {
                let arrived = &mut self.arrived;
                let plane = &mut self.liveness;
                let (n, observed) = match self.socket.receive_observed(|a, bytes| {
                    // The driver epoch is fixed. A deschedule before, during or after the
                    // drain cannot shift the time at which a queued heartbeat arrived.
                    let heartbeat = super::heartbeat::Plane::recognizes(bytes);
                    if let Some(stamp) = a.kernel_at_ns.and_then(|at| epoch?.checked_add(at)) {
                        through = Some(stamp);
                        if heartbeat && let Some(plane) = plane { plane.receive(a.from,stamp,bytes); }
                    }
                    if !heartbeat {
                        arrived.push((a.from, BytesMut::from(&*bytes)));
                    }
                }) {
                    Ok(n) => n,
                    Err(e) => return Poll::Ready(Err(e)),
                };
                got += n;
                if observed == hyper_rt::udp::Queued::Emptied {
                    through = Some(through.map_or(judge_at, |at| judge_at.max(at)));
                    break;
                }
                if n == 0 {moved = true; break;}
                if got >= BATCH * 4 {break;}
            }
            let mut arrived = std::mem::take(&mut self.arrived);
            for (from,data) in arrived.drain(..) { self.on_datagram(now,from,data); }
            if let (Some(at),Some(plane))=(through,&mut self.liveness) { plane.drive(at,false); }
            self.arrived = arrived;

            // 3. Fire due QUIC connection timers and reap half-open dials past `DIAL_TIMEOUT`
            // ONCE (setting `dial_timed_out = true` and continuing to service QUIC's close timer so
            // a timed-out dial never pins the driver loop).
            for i in 0..self.conns.len() {
                if let Some(Some(c)) = self.conns.get_mut(i) && !c.connected && !c.dial_timed_out && now >= c.dial_due {
                    c.dial_timed_out = true;
                    if let Some(peer) = &c.peer_name {
                        let timing = self.dial_time.entry(peer.clone()).or_default();
                        timing.expired();
                        if self.by_peer.get(peer) == Some(&i) {
                            self.failed_until.lock().insert(peer.clone(), now + timing.timeout());
                        }
                    }
                    for p in c.pending_out.drain(..) {
                        if let Some(r) = p.reply { let _ = r.send(Err(anyhow::anyhow!("direct QUIC handshake timed out"))); }
                    }
                    c.quic.close(now, VarInt::from_u32(0), bytes::Bytes::from_static(b"dial timeout"));
                    self.after_conn(i, now);
                }
                let due = self.conns.get_mut(i).and_then(|c| c.as_mut()).and_then(|c| c.quic.poll_timeout());
                if due.is_some_and(|d| d <= now) {
                    if let Some(Some(c)) = self.conns.get_mut(i) {
                        c.quic.handle_timeout(now);
                    }
                    self.after_conn(i, now);
                }
            }

            // 4. Queue endpoint-generated replies (VersionNegotiation / Retry / Refuse) and connection transmits.
            for (to, bytes) in self.replies.drain(..) {
                self.socket.queue(to, &bytes);
            }
            let mut sent = 0usize;
            while sent < BATCH && self.socket.slot().is_some() {
                let Some(t) = self.poll_transmit(now) else { break };
                self.socket.queue(t.destination, self.out_buf.get(..t.size).unwrap_or_default());
                sent += 1;
            }
            let blocked = self.socket.send() == Sent::Blocked;
            let mut beats_wait = false;
            let mut beat_blocked = false;
            if let Some(plane) = &mut self.liveness {
                beats_wait = plane.send(&mut self.socket);
                if self.socket.pending() && !blocked { beat_blocked = self.socket.send() == Sent::Blocked; }
            }
            if moved || got > 0 || ((sent == BATCH || beats_wait) && !blocked && !beat_blocked) {
                continue;
            }

            // 5. Park until socket readable/writable, command wake, or next QUIC timer.
            let readable = self.readable.get_or_insert_with(|| self.socket.readable());
            if let Poll::Ready(r) = Pin::new(readable).poll(cx) {
                self.readable = None;
                if let Err(e) = r {
                    return Poll::Ready(Err(e));
                }
                continue;
            }
            if self.socket.pending() {
                let writable = self.writable.get_or_insert_with(|| self.socket.writable());
                if let Poll::Ready(r) = Pin::new(writable).poll(cx) {
                    self.writable = None;
                    if let Err(e) = r {
                        return Poll::Ready(Err(e));
                    }
                    continue;
                }
            }
            // Arm wakeup future for command arrivals.
            let w = wake_fut.get_or_insert_with(|| {
                let notify = Arc::clone(wake);
                Box::pin(async move {
                    notify.notified().await;
                })
            });
            if w.as_mut().poll(cx).is_ready() {
                *wake_fut = None;
                continue;
            }
            if let Some(due) = self.next_timeout().into_iter().chain(self.liveness.as_ref().and_then(super::heartbeat::Plane::deadline)).min() {
                if self.timer.as_ref().is_none_or(|(armed, _)| *armed != due) {
                    let wait = u64::try_from(due.saturating_duration_since(now).as_nanos()).unwrap_or(u64::MAX);
                    self.timer = Some((due, sleep(wait)));
                }
                if let Some((_, timer)) = self.timer.as_mut()
                    && let Poll::Ready(r) = Pin::new(timer).poll(cx)
                {
                    self.timer = None;
                    if let Err(e) = r {
                        return Poll::Ready(Err(e));
                    }
                    continue;
                }
            }
            return Poll::Pending;
        }
        cx.waker().wake_by_ref();
        Poll::Pending
    }

    fn on_cmd(&mut self, cmd: Cmd, now: Instant) {
        match cmd {
            Cmd::UpdatePeers(list) => {
                if let Some(plane)=&mut self.liveness {plane.directory(&list);}
                let mut fp_map = self.by_fp.lock();
                fp_map.clear();
                self.peers.clear();
                for p in list {
                    let fp = p.fingerprint.to_ascii_lowercase();
                    fp_map.insert(fp, p.name.clone());
                    self.peers.insert(p.name.clone(), p);
                }
                self.dial_time.retain(|p,_| self.peers.contains_key(p));
                self.addr_cursor.retain(|p,_| self.peers.contains_key(p));
                self.failed_until.lock().retain(|p,_| self.peers.contains_key(p));
                drop(fp_map);
                for cid in 0..self.conns.len() {
                    let Some(Some(c)) = self.conns.get_mut(cid) else { continue };
                    if c.connected && c.fingerprint.as_ref().and_then(|fp| self.by_fp.lock().get(fp).cloned()) != c.peer_name {
                        if let Some(peer) = &c.peer_name { self.connected_peers.lock().remove(peer); }
                        c.connected = false;
                        c.dial_timed_out = true;
                        c.quic.close(now, VarInt::from_u32(0), bytes::Bytes::from_static(b"fleet identity changed"));
                        self.mark(cid);
                    }
                }
            }
            Cmd::ReapCanceled(peer) => {
                for cid in 0..self.conns.len() {
                    let Some(Some(c)) = self.conns.get_mut(cid) else { continue };
                    if c.peer_name.as_deref() != Some(peer.as_str()) { continue; }
                    c.pending_out.retain(|p| !p.reply.as_ref().is_some_and(oneshot::Sender::is_canceled));
                    c.streams.retain(|id, sb| {
                        if !sb.reply.as_ref().is_some_and(oneshot::Sender::is_canceled) { return true; }
                        let _ = c.quic.send_stream(*id).reset(VarInt::from_u32(0));
                        let _ = c.quic.recv_stream(*id).stop(VarInt::from_u32(0));
                        false
                    });
                    self.mark(cid);
                }
            }
            Cmd::WarmPeer(peer) => {
                self.warm_peer_conn(&peer, now);
            }
            Cmd::SendHarness { peer, payload, reply } => {
                self.enqueue_outbound(&peer, PendingOut { payload, reply: Some(reply) }, now);
            }
            Cmd::SendConvoRaft { peer, payload } => {
                self.enqueue_outbound(&peer, PendingOut { payload, reply: None }, now);
            }
            Cmd::StreamReply { conn_id, incarnation, stream_id, frame } => {
                if let Some(Some(c)) = self.conns.get_mut(conn_id)
                    && Arc::ptr_eq(&incarnation, &c.incarnation)
                    && let Some(sb) = c.streams.get_mut(&stream_id)
                {
                    if !write_reply(&mut c.quic, stream_id, sb, frame) { c.streams.remove(&stream_id); }
                    self.mark(conn_id);
                }
            }
        }
    }

    fn warm_peer_conn(&mut self, peer: &str, now: Instant) {
        if let Some(&cid) = self.by_peer.get(peer)
            && let Some(Some(c)) = self.conns.get(cid)
            && !c.quic.is_closed()
        {
            return;
        }
        let Some(entry) = self.peers.get(peer).cloned() else { return };
        if entry.addrs.is_empty() {
            return;
        }
        let cursor = self.addr_cursor.entry(peer.to_owned()).or_insert(0);
        let idx = *cursor % entry.addrs.len();
        *cursor = cursor.wrapping_add(1);
        let Some(addr) = entry.addrs.get(idx).copied() else { return };
        let Some(sni) = fp_server_name(&entry.fingerprint) else { return };
        if let Ok((handle, quic)) = self.endpoint.connect(now, self.client_handle, addr, &sni, None) {
            let cid = handle.0;
            if self.conns.len() <= cid {
                self.conns.resize_with(cid + 1, || None);
            }
            let pc = PeerConn {
                incoming: Arc::clone(&self.incoming),
                incarnation: Arc::new(()),
                fingerprint: None,
                handle,
                quic,
                peer_name: Some(peer.to_owned()),
                connected: false,
                dial_timed_out: false,
                started_at: now,
                dial_due: now + self.dial_time.get(peer).map_or(DIAL_TIMEOUT, crate::harness::told::Rto::timeout),
                dirty: false,
                pending_out: VecDeque::new(),
                streams: HashMap::new(),
            };
            if let Some(slot) = self.conns.get_mut(cid) {
                *slot = Some(pc);
            }
            self.by_peer.insert(peer.to_owned(), cid);
            self.after_conn(cid, now);
        }
    }

    fn enqueue_outbound(&mut self, peer: &str, item: PendingOut, now: Instant) {
        if let Some(&cid) = self.by_peer.get(peer)
            && let Some(Some(c)) = self.conns.get_mut(cid)
            && !c.quic.is_closed()
        {
            c.pending_out.push_back(item);
            if c.connected {
                open_pending_streams(c);
            }
            self.mark(cid);
            return;
        }
        let Some(entry) = self.peers.get(peer).cloned() else {
            if let Some(r) = item.reply {
                let _ = r.send(Err(anyhow::anyhow!("no direct QUIC entry for peer {peer}")));
            }
            return;
        };
        if entry.addrs.is_empty() {
            if let Some(r) = item.reply {
                let _ = r.send(Err(anyhow::anyhow!("peer {peer} advertises no direct QUIC address")));
            }
            return;
        }
        let cursor = self.addr_cursor.entry(peer.to_owned()).or_insert(0);
        let idx = *cursor % entry.addrs.len();
        *cursor = cursor.wrapping_add(1);
        let Some(addr) = entry.addrs.get(idx).copied() else {
            if let Some(r) = item.reply {
                let _ = r.send(Err(anyhow::anyhow!("peer {peer} advertises no direct QUIC address")));
            }
            return;
        };
        let Some(sni) = fp_server_name(&entry.fingerprint) else {
            if let Some(r) = item.reply {
                let _ = r.send(Err(anyhow::anyhow!("peer {peer} has invalid fingerprint")));
            }
            return;
        };
        match self.endpoint.connect(now, self.client_handle, addr, &sni, None) {
            Ok((handle, quic)) => {
                let cid = handle.0;
                if self.conns.len() <= cid {
                    self.conns.resize_with(cid + 1, || None);
                }
                let mut pc = PeerConn {
                    incoming: Arc::clone(&self.incoming),
                    incarnation: Arc::new(()),
                    fingerprint: None,
                    handle,
                    quic,
                    peer_name: Some(peer.to_owned()),
                    connected: false,
                    dial_timed_out: false,
                    started_at: now,
                    dial_due: now + self.dial_time.get(peer).map_or(DIAL_TIMEOUT, crate::harness::told::Rto::timeout),
                    dirty: false,
                    pending_out: VecDeque::new(),
                    streams: HashMap::new(),
                };
                pc.pending_out.push_back(item);
                if let Some(slot) = self.conns.get_mut(cid) {
                    *slot = Some(pc);
                }
                self.by_peer.insert(peer.to_owned(), cid);
                self.after_conn(cid, now);
            }
            Err(e) => {
                if let Some(r) = item.reply {
                    let _ = r.send(Err(anyhow::anyhow!("direct QUIC connect to {peer} ({addr}): {e:?}")));
                }
            }
        }
    }

    fn on_datagram(&mut self, now: Instant, from: SocketAddr, data: BytesMut) {
        self.out_buf.clear();
        let Some(ev) = self.endpoint.handle(now, from, None, None, data, &mut self.out_buf) else {
            return;
        };
        match ev {
            DatagramEvent::Response(t) => {
                self.replies.push((t.destination, self.out_buf.get(..t.size).unwrap_or_default().to_vec()));
            }
            DatagramEvent::ConnectionEvent(h, cev) => {
                let cid = h.0;
                if let Some(Some(c)) = self.conns.get_mut(cid) {
                    c.quic.handle_event(cev, self.endpoint.configs_mut());
                }
                self.after_conn(cid, now);
            }
            DatagramEvent::NewConnection(incoming) => {
                self.out_buf.clear();
                match self.endpoint.accept(incoming, now, &mut self.out_buf, None, None) {
                    Ok((handle, quic)) => {
                        let cid = handle.0;
                        if self.conns.len() <= cid {
                            self.conns.resize_with(cid + 1, || None);
                        }
                        if let Some(slot) = self.conns.get_mut(cid) {
                            *slot = Some(PeerConn {
                                incoming: Arc::clone(&self.incoming),
                                incarnation: Arc::new(()),
                                fingerprint: None,
                                handle,
                                quic,
                                peer_name: None,
                                connected: false,
                                dial_timed_out: false,
                                started_at: now,
                                dial_due: now + DIAL_TIMEOUT,
                                dirty: false,
                                pending_out: VecDeque::new(),
                                streams: HashMap::new(),
                            });
                        }
                        self.after_conn(cid, now);
                    }
                    Err(e) => {
                        if let Some(t) = e.response {
                            self.replies.push((t.destination, self.out_buf.get(..t.size).unwrap_or_default().to_vec()));
                        }
                    }
                }
            }
        }
    }

    fn after_conn(&mut self, cid: usize, now: Instant) {
        let mut closed_peer: Option<String> = None;
        let mut newly_identified: Option<String> = None;
        {
            let Some(Some(c)) = self.conns.get_mut(cid) else { return };
            while let Some(ev) = c.quic.poll() {
                match ev {
                    QuicEvent::Connected => {
                        let fingerprint = c.quic.crypto_session().peer_identity()
                            .and_then(|id| id.downcast::<Vec<CertificateDer<'static>>>().ok())
                            .and_then(|certs| e2e::fingerprint_of_cert(certs.first()?));
                        let name = fingerprint.as_ref().and_then(|fp| self.by_fp.lock().get(fp).cloned());
                        let Some(name) = name.filter(|name| c.peer_name.as_ref().is_none_or(|expected| expected == name)) else {
                            c.quic.close(now, VarInt::from_u32(0), bytes::Bytes::from_static(b"unknown fleet identity"));
                            continue;
                        };
                        if c.peer_name.is_some() {
                            self.dial_time.entry(name.clone()).or_default().sample(u64::try_from(now.saturating_duration_since(c.started_at).as_millis()).unwrap_or(u64::MAX));
                        }
                        c.fingerprint = fingerprint;
                        c.connected = true;
                        c.peer_name = Some(name.clone());
                        newly_identified = Some(name.clone());
                        self.failed_until.lock().remove(&name);
                        if self.connected_peers.lock().insert(name.clone()) && let Some(outbox)=self.starts.get().and_then(std::sync::Weak::upgrade) {outbox.linked(&name);}
                        open_pending_streams(c);
                    }
                    QuicEvent::Stream(StreamEvent::Available { .. }) if c.connected => open_pending_streams(c),
                    QuicEvent::Stream(StreamEvent::Finished { id }) => {
                        if let Some(sb) = c.streams.get_mut(&id) {
                            // QUIC now knows the peer has the complete frame. Bytes retained
                            // for retransmission counted against the budget until this event.
                            if let Some(charge) = &mut sb.send_charge { charge.wrote(); }
                            if sb.reply.is_none() { c.streams.remove(&id); }
                        }
                    }
                    QuicEvent::Stream(StreamEvent::Stopped { id, .. }) => {
                        let _ = c.quic.send_stream(id).reset(VarInt::from_u32(0));
                        if let Some(sb) = c.streams.remove(&id) && let Some(reply) = sb.reply {
                            let _ = reply.send(Err(anyhow::anyhow!("destination stopped QUIC stream")));
                        }
                    }
                    QuicEvent::Stream(StreamEvent::Opened { dir }) => {
                        while let Some(s) = c.quic.streams().accept(dir) {
                            c.streams.insert(
                                s,
                                StreamBuf {
                                    send_charge: None,
                                    read_room: ReadRoom { room: Arc::clone(&c.incoming), bytes: 0 },
                                    write: Vec::new(),
                                    write_pos: 0,
                                    finish_write: false,
                                    read: Vec::new(),
                                    reply: None,
                                    inbound: true,
                                    dispatched: false,
                                },
                            );
                            drain_stream_read(cid, s, c, &self.harness_send, &self.convo_raft, &self.cmd_tx, &self.wake);
                        }
                    }
                    QuicEvent::Stream(StreamEvent::Readable { id }) => {
                        drain_stream_read(cid, id, c, &self.harness_send, &self.convo_raft, &self.cmd_tx, &self.wake);
                    }
                    QuicEvent::Stream(StreamEvent::Writable { id }) => {
                        if let Some(sb) = c.streams.get_mut(&id) && !flush_stream_write(&mut c.quic, id, sb) { c.streams.remove(&id); }
                    }
                    QuicEvent::ConnectionLost { .. } => {
                        c.connected = false;
                        c.dial_timed_out = true;
                        closed_peer = c.peer_name.clone();
                        for p in c.pending_out.drain(..) {
                            if let Some(r) = p.reply {
                                let _ = r.send(Err(anyhow::anyhow!("direct QUIC connection terminated")));
                            }
                        }
                        for (_, mut sb) in c.streams.drain() {
                            if let Some(r) = sb.reply.take() {
                                let _ = r.send(Err(anyhow::anyhow!("direct QUIC stream closed before reply")));
                            }
                        }
                    }
                    _ => {}
                }
            }
            while let Some(ev) = c.quic.poll_endpoint_events() {
                if let Some(back) = self.endpoint.handle_event(c.handle, ev) {
                    c.quic.handle_event(back, self.endpoint.configs_mut());
                }
            }
            if c.quic.is_drained() {
                closed_peer = closed_peer.or_else(|| c.peer_name.clone());
                if let Some(slot) = self.conns.get_mut(cid) {
                    *slot = None;
                }
            }
        }
        if let Some(name) = newly_identified {
            // A restarted member proves the same pin on a new connection while its dead
            // path can still look established until QUIC's idle timeout. Send fresh frames
            // on the newer authenticated path. Existing streams finish on their own path.
            let current = self.by_peer.get(&name).and_then(|i| self.conns.get(*i)?.as_ref());
            let new = self.conns.get(cid).and_then(Option::as_ref);
            if let Some(new) = new && current.is_none_or(|old| !old.connected || new.started_at >= old.started_at) {
                self.by_peer.insert(name, cid);
            }
        }
        if let Some(name) = closed_peer && self.by_peer.get(&name) == Some(&cid) {
            // Crossed dials can leave another healthy authenticated path to the same peer.
            let other = self.conns.iter().enumerate().filter_map(|(id, slot)| {
                let peer = slot.as_ref()?;
                (peer.connected && !peer.quic.is_closed() && peer.peer_name.as_deref() == Some(&name)).then_some((peer.started_at,id))
            }).max();
            if let Some((_, id)) = other {
                self.by_peer.insert(name.clone(), id);
                self.failed_until.lock().remove(&name);
            } else {
                self.by_peer.remove(&name);
                self.connected_peers.lock().remove(&name);
            }
        }
        self.mark(cid);
    }

    fn mark(&mut self, cid: usize) {
        if let Some(Some(c)) = self.conns.get_mut(cid)
            && !c.dirty
        {
            c.dirty = true;
            self.dirty.push_back(cid);
        }
    }

    fn poll_transmit(&mut self, now: Instant) -> Option<hyper_quic::Transmit> {
        while let Some(cid) = self.dirty.pop_front() {
            let Some(Some(c)) = self.conns.get_mut(cid) else { continue };
            self.out_buf.clear();
            let t = c.quic.poll_transmit(now, 1, &mut self.out_buf, self.endpoint.configs());
            match t {
                Some(t) => {
                    self.dirty.push_back(cid);
                    return Some(t);
                }
                None => c.dirty = false,
            }
        }
        None
    }

    fn next_timeout(&mut self) -> Option<Instant> {
        self.conns
            .iter_mut()
            .filter_map(|c| {
                let c = c.as_mut()?;
                let quic_due = c.quic.poll_timeout();
                let dial_due = (!c.connected && !c.dial_timed_out).then_some(c.dial_due);
                match (quic_due, dial_due) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                }
            })
            .min()
    }
}

fn open_pending_streams(c: &mut PeerConn) {
    for _ in 0..c.pending_out.len() {
        let Some(item) = c.pending_out.pop_front() else { break };
        if item.reply.as_ref().is_some_and(oneshot::Sender::is_canceled) { continue; }
        let dir = if item.reply.is_some() { Dir::Bi } else { Dir::Uni };
        let Some(stream_id) = c.quic.streams().open(dir) else {
            // Stream credit temporarily full: put back at front and retry when stream closes.
            c.pending_out.push_back(item);
            continue;
        };
        let mut sb = StreamBuf {
            send_charge: Some(item.payload.charge),
            read_room: ReadRoom { room: Arc::clone(&c.incoming), bytes: 0 },
            write: item.payload.data,
            write_pos: 0,
            finish_write: true,
            read: Vec::new(),
            reply: item.reply,
            inbound: false,
            dispatched: false,
        };
        if !flush_stream_write(&mut c.quic, stream_id, &mut sb) { continue; }
        c.streams.insert(stream_id, sb);
    }
}

fn flush_stream_write(quic: &mut Connection, id: StreamId, sb: &mut StreamBuf) -> bool {
    while sb.write_pos < sb.write.len() {
        let slice = sb.write.get(sb.write_pos..).unwrap_or_default();
        match quic.send_stream(id).write(slice) {
            Ok(0) | Err(WriteError::Blocked) => return true,
            Ok(n) => sb.write_pos = sb.write_pos.saturating_add(n),
            Err(e) => {
                if let Some(reply) = sb.reply.take() { let _ = reply.send(Err(anyhow::anyhow!("QUIC stream write failed: {e:?}"))); }
                sb.write = Vec::new();
                sb.write_pos = 0;
                sb.finish_write = false;
                return false;
            }
        }
    }
    if sb.finish_write {
        if quic.send_stream(id).finish().is_err() { return false; }
        sb.write = Vec::new();
        sb.write_pos = 0;
        sb.finish_write = false;
    }
    true
}

fn drain_stream_read(
    conn_id: usize,
    stream_id: StreamId,
    c: &mut PeerConn,
    harness_send: &Option<crate::memory::queue::Sender<HarnessSendAsk>>,
    convo_raft: &Option<crate::memory::queue::Sender<ConvoRaftIn>>,
    cmd_tx: &crate::memory::queue::Sender<Cmd>,
    wake: &Arc<crate::rt::Notify>,
) {
    let mut fin = false;
    let mut reset = false;
    {
        let Some(sb) = c.streams.get_mut(&stream_id) else { return };
        let mut recv = c.quic.recv_stream(stream_id);
        if let Ok(mut chunks) = recv.read(true) {
            loop {
                match chunks.next(64 << 10) {
                    Ok(Some(chunk)) => {
                        let len = sb.read.len().saturating_add(chunk.bytes.len());
                        if len > MAX_STREAM_PAYLOAD + 5 {
                            reset = true;
                            break;
                        }
                        if len > sb.read_room.bytes {
                            let to = len.next_power_of_two().min(MAX_STREAM_PAYLOAD + 5);
                            if !sb.read_room.room.try_take(to - sb.read_room.bytes) { reset = true; break; }
                            sb.read_room.bytes = to;
                            if sb.read.try_reserve_exact(to - sb.read.len()).is_err() { reset = true; break; }
                        }
                        sb.read.extend_from_slice(&chunk.bytes);
                    }
                    Ok(None) => {
                        fin = true;
                        break;
                    }
                    Err(ReadError::Blocked) => break,
                    Err(ReadError::Reset(_)) => {
                        reset = true;
                        break;
                    }
                }
            }
            let _ = chunks.finalize();
        }
    }
    if reset {
        let _ = c.quic.recv_stream(stream_id).stop(VarInt::from_u32(0));
        if stream_id.dir() == Dir::Bi { let _ = c.quic.send_stream(stream_id).reset(VarInt::from_u32(0)); }
        if let Some(mut sb) = c.streams.remove(&stream_id)
            && let Some(r) = sb.reply.take()
        {
            let _ = r.send(Err(anyhow::anyhow!("direct QUIC stream reset by peer")));
        }
        return;
    }
    if !fin { return; }
    let Some(sb) = c.streams.get_mut(&stream_id) else { return };
    if sb.dispatched {
        return;
    }
    sb.dispatched = true;
    if !sb.inbound {
        // Client side received reply to HarnessSend.
        let reply = parse_reply(&sb.read);
        if let Ok(P2pSendReply::Started { status, .. }) = &reply {
            tracing::debug!(target: "kiln.session_latency", peer = c.peer_name.as_deref(), status,
                rtt_ns = c.quic.rtt().as_nanos(), "direct session start reply");
        }
        if let Some(tx) = sb.reply.take() {
            let _ = tx.send(reply);
        }
        c.streams.remove(&stream_id);
        return;
    }
    // Server side received complete inbound stream from peer.
    let Some(from) = c.peer_name.clone().filter(|_| c.connected && !c.quic.is_closed()) else {
        c.streams.remove(&stream_id);
        return;
    };
    let read = std::mem::take(&mut sb.read);
    let room = Arc::clone(&c.incoming);
    let admitted = std::mem::replace(&mut sb.read_room, ReadRoom { room: room.clone(), bytes: 0 });
    let decoded = read.split_first().and_then(|(&kind, rest)| {
        let (head, body) = rest.split_at_checked(4)?;
        let len = usize::try_from(u32::from_be_bytes(head.try_into().ok()?)).ok()?;
        (body.len() == len && matches!((kind, stream_id.dir()), (KIND_HARNESS_SEND, Dir::Bi) | (KIND_CONVO_RAFT, Dir::Uni))).then_some((kind, body))
    });
    let Some((kind, body)) = decoded else {
        if stream_id.dir() == Dir::Bi { let _ = c.quic.send_stream(stream_id).reset(VarInt::from_u32(0)); }
        c.streams.remove(&stream_id);
        return;
    };
    match kind {
        KIND_HARNESS_SEND => {
            let parsed = postcard::from_bytes::<DirectHarnessSend>(body);
            match (parsed, harness_send) {
                (Ok(ask), Some(tx)) => {
                    let (rtx, rrx) = oneshot::channel();
                    let sent = tx.try_send(HarnessSendAsk { from, session: ask.session, epoch: ask.epoch, json: ask.json, reply: rtx, _room: Some(admitted) }).is_ok();
                    let incarnation = Arc::clone(&c.incarnation);
                    let (cmd_tx, wake) = (cmd_tx.clone(), Arc::clone(wake));
                    if sent {
                        crate::rt::spawn(async move {
                            let reply = match crate::rt::timeout(REQUEST_WAIT, rrx).await {
                                Ok(Ok(reply)) => reply,
                                Ok(Err(_)) => P2pSendReply::Error { why: "target runner dropped send".into() },
                                Err(_) => P2pSendReply::Error { why: "timed out waiting for target receipt; retry with the same message id".into() },
                            };
                            let _ = cmd_tx.try_send(Cmd::StreamReply { conn_id, incarnation, stream_id, frame: frame_reply(&reply, room) });
                            wake.notify();
                        });
                    } else {
                        if !write_reply(&mut c.quic, stream_id, sb, frame_reply(&P2pSendReply::Error { why: "target runner queue full".into() }, room)) { c.streams.remove(&stream_id); }
                    }
                }
                (Ok(_), None) => {
                    if !write_reply(&mut c.quic, stream_id, sb, frame_reply(&P2pSendReply::NoSession, room)) { c.streams.remove(&stream_id); }
                }
                (Err(e), _) => {
                    if !write_reply(&mut c.quic, stream_id, sb, frame_reply(&P2pSendReply::Error { why: format!("malformed direct QUIC send: {e}") }, room)) { c.streams.remove(&stream_id); }
                }
            }
        }
        KIND_CONVO_RAFT => {
            if let Ok(msg) = postcard::from_bytes::<DirectConvoRaft>(body)
                && msg.from == from
                && let Some(tx) = convo_raft
            {
                let _ = tx.try_send(ConvoRaftIn { from, session: msg.session, bytes: msg.bytes, _room: Some(admitted) });
            }
            c.streams.remove(&stream_id);
        }
        _ => {
            c.streams.remove(&stream_id);
        }
    }
}

fn parse_reply(buf: &[u8]) -> anyhow::Result<P2pSendReply> {
    let head = buf.first_chunk::<4>().ok_or_else(|| anyhow::anyhow!("short direct QUIC reply"))?;
    let len = usize::try_from(u32::from_be_bytes(*head))?;
    let body = buf.get(4..).filter(|body| body.len() == len).ok_or_else(|| anyhow::anyhow!("bad direct QUIC reply length"))?;
    serde_json::from_slice(body).context("decode direct QUIC P2pSendReply")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::inbox::Answer;

    #[test]
    fn cp_t3_2_frames_hold_the_byte_and_peer_budgets_until_release() -> anyhow::Result<()> {
        let room = Arc::new(FrameRoom::new());
        let old = DirectConvoRaft {session:"s".into(),from:"p".into(),bytes:(0..=255).collect()};
        let frame = room.encode("p", KIND_CONVO_RAFT, &("s","p",Octets(&old.bytes)))?;
        anyhow::ensure!(frame.data.get(5..) == Some(postcard::to_allocvec(&old)?.as_slice()));
        let mut held = vec![frame];
        for _ in 1..MAX_PENDING_OUT { held.push(room.encode("p",KIND_CONVO_RAFT,&())?); }
        anyhow::ensure!(room.encode("p",KIND_CONVO_RAFT,&()).is_err() && room.encode("q",KIND_CONVO_RAFT,&()).is_ok());
        drop(held);
        anyhow::ensure!(room.counts.lock().is_empty());
        let reply = P2pSendReply::Error { why:"a".repeat(1024) };
        let replies = Arc::new(crate::memory::queue::Room::new(&crate::memory::owners::CHANNELS,128));
        let frame = frame_reply(&reply,replies.clone()).context("budget refusal frame")?;
        anyhow::ensure!(matches!(parse_reply(&frame.data)?,P2pSendReply::Error {why} if why.contains("budget")));
        drop(frame);
        anyhow::ensure!(replies.try_take(128));
        replies.give(128);
        Ok(())
    }

    #[test]
    fn cp_t3_2_restarted_peer_uses_its_new_authenticated_connection() -> anyhow::Result<()> {
        use crate::memory::{owners, queue};
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let a = Identity::load_or_create(&dir.path().join("a"), "a")?;
            let b = Identity::load_or_create(&dir.path().join("b"), "b")?;
            let open = |id: &Identity| -> anyhow::Result<_> {
                let (tx, rx) = queue::unbounded::<ConvoRaftIn>(&owners::CHANNELS);
                Ok((PeerQuic::start("127.0.0.1:0".parse()?, id, None, Some(tx))?, rx))
            };
            let peer = |name: &str, id: &Identity, quic: &PeerQuic| PeerEntry {
                name: name.into(), fingerprint: id.fingerprint.clone(), addrs: vec![quic.local_addr()],
            };
            let (source, source_rx) = open(&a)?;
            let (old, old_rx) = open(&b)?;
            source.update_peers(vec![peer("b", &b, &old)]);
            old.update_peers(vec![peer("a", &a, &source)]);
            anyhow::ensure!(source.send_convo_raft("b", "before", "a", &[1]));
            anyhow::ensure!(crate::rt::timeout(DIAL_TIMEOUT, old_rx.recv()).await??.session == "before");

            // B reopens with its original key and contacts A before A's old connection expires.
            let (new, new_rx) = open(&b)?;
            new.update_peers(vec![peer("a", &a, &source)]);
            anyhow::ensure!(new.send_convo_raft("a", "reconnected", "b", &[2]));
            anyhow::ensure!(crate::rt::timeout(DIAL_TIMEOUT, source_rx.recv()).await??.session == "reconnected");
            anyhow::ensure!(source.send_convo_raft("b", "after", "a", &[3]));
            let frame = futures::future::select(Box::pin(new_rx.recv()), Box::pin(old_rx.recv()));
            match crate::rt::timeout(DIAL_TIMEOUT, frame).await? {
                futures::future::Either::Left((frame, _)) => anyhow::ensure!(frame?.session == "after"),
                futures::future::Either::Right((frame, _)) => anyhow::bail!("sent {} to the retired connection after replacement handshake", frame?.session),
            }
            Ok(())
        })
    }

    #[test]
    fn direct_hyper_quic_forwards_harness_message_and_raft_between_two_runners() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir_a = tempfile::tempdir()?;
            let dir_b = tempfile::tempdir()?;
            let id_a = Identity::load_or_create(dir_a.path(), "laptop-a")?;
            let id_b = Identity::load_or_create(dir_b.path(), "laptop-b")?;

            let (b_send_tx, b_send_rx) = crate::memory::queue::unbounded::<HarnessSendAsk>(&crate::memory::owners::CHANNELS);
            let (a_raft_tx, a_raft_rx) = crate::memory::queue::unbounded::<ConvoRaftIn>(&crate::memory::owners::CHANNELS);

            let quic_a = PeerQuic::start("127.0.0.1:0".parse()?, &id_a, None, Some(a_raft_tx))?;
            let quic_b = PeerQuic::start("127.0.0.1:0".parse()?, &id_b, Some(b_send_tx), None)?;

            quic_a.update_peers(vec![PeerEntry { name: "laptop-b".into(), fingerprint: id_b.fingerprint.clone(), addrs: vec![quic_b.local_addr()] }]);
            quic_b.update_peers(vec![PeerEntry { name: "laptop-a".into(), fingerprint: id_a.fingerprint.clone(), addrs: vec![quic_a.local_addr()] }]);

            // Simulate laptop-b owning "sess-1" (enqueues locally) and NOT owning "sess-other" (NoSession, zero enqueue).
            crate::rt::spawn(async move {
                while let Ok(ask) = b_send_rx.recv().await {
                    let reply = if ask.session == "sess-1" {
                        P2pSendReply::Accepted {
                            answer: Answer { session: ask.session, message: "m1".into(), epoch: ask.epoch.unwrap_or(1), at: 100, seq: 1, repeat: false },
                        }
                    } else {
                        P2pSendReply::NoSession
                    };
                    let _ = ask.reply.send(reply);
                }
            });

            // 1. laptop-a forwards message for sess-1 directly to laptop-b over hyper-quic!
            let r1 = quic_a.send_harness_message("laptop-b", "sess-1", Some(1), r#"{"type":"send","id":"m1","text":"hi"}"#.into()).await?;
            anyhow::ensure!(matches!(r1, P2pSendReply::Accepted { ref answer } if answer.session == "sess-1" && answer.message == "m1"), "expected Accepted over direct hyper-quic: {r1:?}");

            // 2. laptop-a forwards message for sess-other -> laptop-b returns NoSession (never enqueued locally).
            let r2 = quic_a.send_harness_message("laptop-b", "sess-other", Some(1), r#"{"type":"send","id":"m2","text":"hi"}"#.into()).await?;
            anyhow::ensure!(matches!(r2, P2pSendReply::NoSession), "expected NoSession: {r2:?}");

            // 3. laptop-b sends a per-session conversation Raft frame directly to laptop-a over hyper-quic!
            anyhow::ensure!(quic_b.send_convo_raft("laptop-a", "forged", "different-runner", &[0]));
            anyhow::ensure!(quic_b.send_convo_raft("laptop-a", "sess-1", "laptop-b", &[1, 2, 3, 4]));
            let got = crate::rt::timeout(Duration::from_secs(3), a_raft_rx.recv()).await??;
            anyhow::ensure!(got.from == "laptop-b" && got.session == "sess-1" && got.bytes == vec![1, 2, 3, 4], "unexpected raft frame: {} {} {:?}", got.from, got.session, got.bytes);
            let addr = quic_a.local_addr();
            drop(quic_a);
            crate::rt::timeout(Duration::from_secs(3), async {
                while std::net::UdpSocket::bind(addr).is_err() { crate::rt::sleep(Duration::from_millis(1)).await; }
            }).await?;
            Ok(())
        })
    }
}
