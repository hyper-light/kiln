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

use std::collections::{HashMap, VecDeque};
use std::future::{Future as _, poll_fn};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
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
/// Datagram batch per syscall.
const BATCH: usize = 32;
/// Max turns per poll before yielding to the shard.
const TURNS: usize = 64;
/// How long a direct QUIC dial/RPC waits before letting caller fall back to relayed QUIC.
const DIRECT_SEND_TIMEOUT: Duration = Duration::from_secs(4);
/// Idle timeout for direct peer QUIC connections.
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// An inbound routed harness message received over direct `hyper-quic` (or relayed reach link).
pub struct HarnessSendAsk {
    pub from: String,
    pub session: String,
    pub epoch: Option<u64>,
    pub json: String,
    pub reply: oneshot::Sender<P2pSendReply>,
}

/// An inbound per-session conversation Raft frame received from a peer node.
pub struct ConvoRaftIn {
    pub from: String,
    pub session: String,
    pub bytes: Vec<u8>,
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
    t.max_concurrent_bidi_streams(VarInt::from_u32(128))
        .max_concurrent_uni_streams(VarInt::from_u32(64))
        .receive_window(VarInt::from_u32(16 << 20))
        .stream_receive_window(VarInt::from_u32(4 << 20))
        .send_window(16 << 20)
        .max_idle_timeout(hyper_quic::IdleTimeout::try_from(IDLE_TIMEOUT).ok())
        .keep_alive_interval(Some(Duration::from_secs(15)));
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
        let lo = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port).to_string();
        if !addrs.contains(&lo) {
            addrs.push(lo);
        }
    } else {
        addrs.push(local.to_string());
    }
    addrs
}

/// Commands into the direct `hyper-quic` driver task.
enum Cmd {
    UpdatePeers(Vec<PeerEntry>),
    SendHarness {
        peer: String,
        payload: Vec<u8>,
        reply: oneshot::Sender<anyhow::Result<P2pSendReply>>,
    },
    SendConvoRaft {
        peer: String,
        payload: Vec<u8>,
    },
    StreamReply {
        conn_id: usize,
        stream_id: StreamId,
        bytes: Vec<u8>,
    },
}

/// Handle to this runner's direct `hyper-quic` forwarding endpoint (pointer-sized `Arc` handle).
#[derive(Clone)]
pub struct PeerQuic(Arc<PeerQuicInner>);

struct PeerQuicInner {
    tx: crate::memory::queue::Sender<Cmd>,
    wake: Arc<crate::rt::Notify>,
    local_addr: SocketAddr,
    advertised: Vec<String>,
    known_peers: Arc<Mutex<HashMap<String, Vec<SocketAddr>>>>,
}

impl PeerQuic {
    /// Start a direct `hyper-quic` listener on `bind` (`0.0.0.0:0` or `127.0.0.1:0` in tests).
    pub fn start(
        bind: SocketAddr,
        identity: &Identity,
        harness_send: Option<crate::memory::queue::Sender<HarnessSendAsk>>,
        convo_raft: Option<crate::memory::queue::Sender<ConvoRaftIn>>,
    ) -> anyhow::Result<Self> {
        let by_fp: Arc<Mutex<HashMap<String, String>>> = Arc::default();
        let server_cfg = build_server_config(identity, Arc::clone(&by_fp))?;
        let client_cfg = build_client_config(identity)?;
        let mut endpoint = Endpoint::new(EndpointConfig::default(), Some(server_cfg), true, None)
            .map_err(|e| anyhow::anyhow!("create p2p hyper-quic endpoint: {e:?}"))?;
        let client_handle = endpoint.insert_client_config(client_cfg).map_err(|e| anyhow::anyhow!("insert p2p client config: {e:?}"))?;
        let socket = Batched::bind(bind, Io { batch: BATCH }, false).map_err(|e| anyhow::anyhow!("bind p2p hyper-quic socket {bind}: {e}"))?;
        let local_addr = socket.local_addr().map_err(|e| anyhow::anyhow!("p2p hyper-quic local_addr: {e}"))?;
        let advertised = advertised_addrs(local_addr);
        let (tx, rx) = crate::memory::queue::unbounded::<Cmd>(&crate::memory::owners::CHANNELS);
        let wake = Arc::new(crate::rt::Notify::new());
        let known_peers: Arc<Mutex<HashMap<String, Vec<SocketAddr>>>> = Arc::default();

        let state = DriverState {
            endpoint,
            client_handle,
            socket,
            by_fp,
            peers: HashMap::new(),
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
            let _ = state.run().await;
        });

        Ok(Self(Arc::new(PeerQuicInner { tx, wake, local_addr, advertised, known_peers })))
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.0.local_addr
    }

    pub fn advertised_addrs(&self) -> &[String] {
        &self.0.advertised
    }

    /// Update the peer directory (fingerprint + direct UDP `quic_addrs`).
    pub fn update_peers(&self, peers: Vec<PeerEntry>) {
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

    /// Whether `peer` currently advertises at least one direct `hyper-quic` UDP endpoint.
    pub fn has_direct(&self, peer: &str) -> bool {
        self.0.known_peers.lock().get(peer).is_some_and(|a| !a.is_empty())
    }

    /// Forward a routed harness message directly to `peer` over a bidirectional `hyper-quic` stream.
    pub async fn send_harness_message(&self, peer: &str, session: &str, epoch: Option<u64>, json: String) -> anyhow::Result<P2pSendReply> {
        let body = postcard::to_allocvec(&DirectHarnessSend { session: session.to_owned(), epoch, json }).context("encode direct QUIC harness send")?;
        let payload = frame_stream(KIND_HARNESS_SEND, &body)?;
        let (reply_tx, reply_rx) = oneshot::channel();
        self.0
            .tx
            .try_send(Cmd::SendHarness { peer: peer.to_owned(), payload, reply: reply_tx })
            .map_err(|_| anyhow::anyhow!("direct QUIC driver stopped"))?;
        self.0.wake.notify();
        match crate::rt::timeout(DIRECT_SEND_TIMEOUT, reply_rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => anyhow::bail!("direct QUIC request to {peer} dropped"),
            Err(_) => anyhow::bail!("direct QUIC send to {peer} timed out"),
        }
    }

    /// Send a per-session conversation Raft message directly to `peer` over `hyper-quic` (best-effort;
    /// returns `false` if `peer` has no known direct QUIC address so caller uses relayed QUIC).
    pub fn send_convo_raft(&self, peer: &str, session: &str, from: &str, bytes: Vec<u8>) -> bool {
        if !self.has_direct(peer) {
            return false;
        }
        let Ok(body) = postcard::to_allocvec(&DirectConvoRaft { session: session.to_owned(), from: from.to_owned(), bytes }) else {
            return false;
        };
        let Ok(payload) = frame_stream(KIND_CONVO_RAFT, &body) else {
            return false;
        };
        let sent = self.0.tx.try_send(Cmd::SendConvoRaft { peer: peer.to_owned(), payload }).is_ok();
        if sent {
            self.0.wake.notify();
        }
        sent
    }
}

fn frame_stream(kind: u8, body: &[u8]) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(body.len() <= MAX_STREAM_PAYLOAD, "direct QUIC payload {} exceeds limit", body.len());
    let len = u32::try_from(body.len())?;
    let mut out = Vec::with_capacity(5 + body.len());
    out.push(kind);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(body);
    Ok(out)
}

fn frame_reply(reply: &P2pSendReply) -> Vec<u8> {
    let body = serde_json::to_vec(reply).unwrap_or_else(|_| br#"{"t":"error","why":"encode failed"}"#.to_vec());
    let len = u32::try_from(body.len()).unwrap_or(0);
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&body);
    out
}

struct PendingOut {
    payload: Vec<u8>,
    reply: Option<oneshot::Sender<anyhow::Result<P2pSendReply>>>,
}

struct StreamBuf {
    write: Vec<u8>,
    write_pos: usize,
    finish_write: bool,
    read: Vec<u8>,
    reply: Option<oneshot::Sender<anyhow::Result<P2pSendReply>>>,
    inbound: bool,
    dispatched: bool,
}

struct PeerConn {
    handle: ConnectionHandle,
    quic: Connection,
    peer_name: Option<String>,
    connected: bool,
    dirty: bool,
    pending_out: VecDeque<PendingOut>,
    streams: HashMap<StreamId, StreamBuf>,
}

struct DriverState {
    endpoint: Endpoint,
    client_handle: ClientConfigHandle,
    socket: Batched,
    by_fp: Arc<Mutex<HashMap<String, String>>>,
    peers: HashMap<String, PeerEntry>,
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
            let now = Instant::now();
            // 1. Process incoming commands from local callers and inbound stream reply tasks.
            let mut moved = false;
            while let Ok(cmd) = self.cmd_rx.try_recv() {
                moved = true;
                self.on_cmd(cmd, now);
            }

            // 2. Receive UDP datagrams from the socket.
            let mut got = 0usize;
            loop {
                let arrived = &mut self.arrived;
                let n = match self.socket.receive(|a, bytes| arrived.push((a.from, BytesMut::from(&*bytes)))) {
                    Ok(n) => n,
                    Err(e) => return Poll::Ready(Err(e)),
                };
                got += n;
                if n == 0 || self.arrived.len() >= BATCH * 4 {
                    break;
                }
            }
            for (from, data) in std::mem::take(&mut self.arrived) {
                self.on_datagram(now, from, data);
            }

            // 3. Fire due QUIC connection timers.
            let ids: Vec<usize> = self.conns.iter().enumerate().filter_map(|(i, c)| c.as_ref().map(|_| i)).collect();
            for i in ids {
                let due = self.conns.get_mut(i).and_then(|c| c.as_mut()).and_then(|c| c.quic.poll_timeout());
                if due.is_some_and(|d| d <= now) {
                    if let Some(Some(c)) = self.conns.get_mut(i) {
                        c.quic.handle_timeout(now);
                    }
                    self.after_conn(i, now);
                }
            }

            // 4. Queue endpoint-generated replies (VersionNegotiation / Retry / Refuse) and connection transmits.
            for (to, bytes) in std::mem::take(&mut self.replies) {
                self.socket.queue(to, &bytes);
            }
            let mut sent = 0usize;
            while sent < BATCH {
                let Some(t) = self.poll_transmit(now) else { break };
                self.socket.queue(t.destination, self.out_buf.get(..t.size).unwrap_or_default());
                sent += 1;
            }
            let blocked = self.socket.send() == Sent::Blocked;
            if moved || got > 0 || (sent == BATCH && !blocked) {
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
            if let Some(due) = self.next_timeout() {
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
                let mut fp_map = self.by_fp.lock();
                fp_map.clear();
                self.peers.clear();
                for p in list {
                    fp_map.insert(p.fingerprint.clone(), p.name.clone());
                    self.peers.insert(p.name.clone(), p);
                }
            }
            Cmd::SendHarness { peer, payload, reply } => {
                self.enqueue_outbound(&peer, PendingOut { payload, reply: Some(reply) }, now);
            }
            Cmd::SendConvoRaft { peer, payload } => {
                self.enqueue_outbound(&peer, PendingOut { payload, reply: None }, now);
            }
            Cmd::StreamReply { conn_id, stream_id, bytes } => {
                if let Some(Some(c)) = self.conns.get_mut(conn_id)
                    && let Some(sb) = c.streams.get_mut(&stream_id)
                {
                    sb.write = bytes;
                    sb.write_pos = 0;
                    sb.finish_write = true;
                    flush_stream_write(&mut c.quic, stream_id, sb);
                    self.mark(conn_id);
                }
            }
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
        let Some(addr) = entry.addrs.first().copied() else {
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
                    handle,
                    quic,
                    peer_name: Some(peer.to_owned()),
                    connected: false,
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
                                handle,
                                quic,
                                peer_name: None,
                                connected: false,
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

    fn after_conn(&mut self, cid: usize, _now: Instant) {
        let mut closed_peer: Option<String> = None;
        let mut newly_identified: Option<String> = None;
        {
            let Some(Some(c)) = self.conns.get_mut(cid) else { return };
            while let Some(ev) = c.quic.poll() {
                match ev {
                    QuicEvent::Connected => {
                        c.connected = true;
                        if c.peer_name.is_none()
                            && let Some(id_any) = c.quic.crypto_session().peer_identity()
                            && let Ok(certs) = id_any.downcast::<Vec<CertificateDer<'static>>>()
                            && let Some(leaf) = certs.first()
                            && let Some(fp) = e2e::fingerprint_of_cert(leaf)
                            && let Some(name) = self.by_fp.lock().get(&fp).cloned()
                        {
                            c.peer_name = Some(name.clone());
                            newly_identified = Some(name);
                        }
                        open_pending_streams(c);
                    }
                    QuicEvent::Stream(StreamEvent::Opened { dir }) => {
                        while let Some(s) = c.quic.streams().accept(dir) {
                            c.streams.insert(
                                s,
                                StreamBuf {
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
                        if let Some(sb) = c.streams.get_mut(&id) {
                            flush_stream_write(&mut c.quic, id, sb);
                        }
                    }
                    QuicEvent::ConnectionLost { .. } => {
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
            self.by_peer.entry(name).or_insert(cid);
        }
        if let Some(name) = closed_peer
            && self.by_peer.get(&name) == Some(&cid)
        {
            self.by_peer.remove(&name);
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
        self.conns.iter_mut().filter_map(|c| c.as_mut().and_then(|c| c.quic.poll_timeout())).min()
    }
}

fn open_pending_streams(c: &mut PeerConn) {
    while let Some(item) = c.pending_out.pop_front() {
        let dir = if item.reply.is_some() { Dir::Bi } else { Dir::Uni };
        let Some(stream_id) = c.quic.streams().open(dir) else {
            // Stream credit temporarily full: put back at front and retry when stream closes.
            c.pending_out.push_front(item);
            return;
        };
        let mut sb = StreamBuf {
            write: item.payload,
            write_pos: 0,
            finish_write: true,
            read: Vec::new(),
            reply: item.reply,
            inbound: false,
            dispatched: false,
        };
        flush_stream_write(&mut c.quic, stream_id, &mut sb);
        if sb.reply.is_some() || sb.write_pos < sb.write.len() || sb.finish_write {
            c.streams.insert(stream_id, sb);
        }
    }
}

fn flush_stream_write(quic: &mut Connection, id: StreamId, sb: &mut StreamBuf) {
    while sb.write_pos < sb.write.len() {
        let slice = sb.write.get(sb.write_pos..).unwrap_or_default();
        match quic.send_stream(id).write(slice) {
            Ok(0) | Err(WriteError::Blocked) => return,
            Ok(n) => sb.write_pos = sb.write_pos.saturating_add(n),
            Err(_) => {
                sb.finish_write = false;
                return;
            }
        }
    }
    if sb.finish_write {
        let _ = quic.send_stream(id).finish();
        sb.finish_write = false;
    }
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
                        if sb.read.len().saturating_add(chunk.bytes.len()) > MAX_STREAM_PAYLOAD + 16 {
                            reset = true;
                            break;
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
        if let Some(mut sb) = c.streams.remove(&stream_id)
            && let Some(r) = sb.reply.take()
        {
            let _ = r.send(Err(anyhow::anyhow!("direct QUIC stream reset by peer")));
        }
        return;
    }
    if !fin {
        return;
    }
    let Some(sb) = c.streams.get_mut(&stream_id) else { return };
    if sb.dispatched {
        return;
    }
    sb.dispatched = true;
    if !sb.inbound {
        // Client side received reply to HarnessSend.
        let reply = parse_reply(&sb.read);
        if let Some(tx) = sb.reply.take() {
            let _ = tx.send(reply);
        }
        c.streams.remove(&stream_id);
        return;
    }
    // Server side received complete inbound stream from peer.
    let from = c.peer_name.clone().unwrap_or_default();
    let Some((&kind, rest)) = sb.read.split_first() else {
        c.streams.remove(&stream_id);
        return;
    };
    let Some(head) = rest.first_chunk::<4>() else {
        c.streams.remove(&stream_id);
        return;
    };
    let len = usize::try_from(u32::from_be_bytes(*head)).unwrap_or(usize::MAX);
    let Some(body) = rest.get(4..4 + len) else {
        c.streams.remove(&stream_id);
        return;
    };
    match kind {
        KIND_HARNESS_SEND => {
            let parsed = postcard::from_bytes::<DirectHarnessSend>(body);
            match (parsed, harness_send) {
                (Ok(ask), Some(tx)) => {
                    let (rtx, rrx) = oneshot::channel();
                    let sent = tx.try_send(HarnessSendAsk { from, session: ask.session, epoch: ask.epoch, json: ask.json, reply: rtx }).is_ok();
                    let (cmd_tx, wake) = (cmd_tx.clone(), Arc::clone(wake));
                    if sent {
                        crate::rt::spawn(async move {
                            let reply = rrx.await.unwrap_or_else(|_| P2pSendReply::Error { why: "target runner dropped send".into() });
                            let _ = cmd_tx.try_send(Cmd::StreamReply { conn_id, stream_id, bytes: frame_reply(&reply) });
                            wake.notify();
                        });
                    } else {
                        sb.write = frame_reply(&P2pSendReply::Error { why: "target runner queue full".into() });
                        sb.write_pos = 0;
                        sb.finish_write = true;
                        flush_stream_write(&mut c.quic, stream_id, sb);
                    }
                }
                (Ok(_), None) => {
                    sb.write = frame_reply(&P2pSendReply::NoSession);
                    sb.write_pos = 0;
                    sb.finish_write = true;
                    flush_stream_write(&mut c.quic, stream_id, sb);
                }
                (Err(e), _) => {
                    sb.write = frame_reply(&P2pSendReply::Error { why: format!("malformed direct QUIC send: {e}") });
                    sb.write_pos = 0;
                    sb.finish_write = true;
                    flush_stream_write(&mut c.quic, stream_id, sb);
                }
            }
        }
        KIND_CONVO_RAFT => {
            if let Ok(msg) = postcard::from_bytes::<DirectConvoRaft>(body)
                && let Some(tx) = convo_raft
            {
                let sender = if !from.is_empty() { from } else { msg.from };
                let _ = tx.try_send(ConvoRaftIn { from: sender, session: msg.session, bytes: msg.bytes });
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
    let body = buf.get(4..4 + len).ok_or_else(|| anyhow::anyhow!("truncated direct QUIC reply"))?;
    serde_json::from_slice(body).context("decode direct QUIC P2pSendReply")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::inbox::Answer;

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
            anyhow::ensure!(quic_b.send_convo_raft("laptop-a", "sess-1", "laptop-b", vec![1, 2, 3, 4]));
            let got = crate::rt::timeout(Duration::from_secs(3), a_raft_rx.recv()).await??;
            anyhow::ensure!(got.from == "laptop-b" && got.session == "sess-1" && got.bytes == vec![1, 2, 3, 4], "unexpected raft frame: {} {} {:?}", got.from, got.session, got.bytes);
            Ok(())
        })
    }
}
