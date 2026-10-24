//! hyper-quic against upstream quinn-proto 0.11.18, unmodified: the QUIC protocol focal's transport
//! runs (focal-node and focal-wire take quinn 0.11, whose protocol is quinn-proto; `docs/STATUS.md`'s
//! owed bench), in one process. One driver moves both through the same calls over memory:
//!
//! - **a handshake**, client and server together, each timed on its own: a full one (the client
//!   keeps no session) and one resumed by ticket (the client's cache holds the last connection's);
//! - **an exchange**: on one established connection, a request and a reply of a size each way on a
//!   bidirectional stream of their own (focal's shape, a stream per request), each timed;
//! - **a bulk transfer's time a MiB**: one stream, client to server.
//!
//! Both run the same TLS, hyper-tls and upstream rustls 0.23.45, measured at parity by hyper-tls's
//! own `vs_upstream`. The provider on both sides is aws-lc-rs restricted to one group,
//! X25519MLKEM768, and to hyper-quic's two 256-bit suites, so both offer and agree the same.
//! hyper-quic runs twice: with its defaults, and with the two defaults that change what it sends
//! off (the handshake's copies, `TransportConfig::handshake_copies`, and Careful Resume with its
//! warm-up, `EndpointConfig::careful_resume`), so its wire is upstream's and what differs is the
//! implementation. Upstream keeps its defaults.
//!
//! ```text
//! cargo bench -p hyper-quic --bench vs_upstream -- [ROUNDS]
//! ```
//!
//! **Time.** The connections run on a virtual clock: a datagram crosses memory at once, and the
//! clock moves only to the earliest timer once nothing can move, so both sides see the same times
//! for the same events. Only the host's clock times an operation.
//!
//! **Rounds.** The three run in turn, `ROUNDS` times (5 by default), the order rotated each round.
//! Each handshake shape runs the samples a p99 needs for an upper end at 95% coverage
//! (`docs/tails.md` §3.3), shared among the rounds and pooled, and each quantile is stated with its
//! interval. A handshake's account includes closing the connection and draining it on both sides.
//!
//! **What is counted.** Beside the wall clock, which a loaded host moves between the two sides'
//! turns, the bench reads the process's own account (`hyper_measure::usage`): CPU time and, where
//! the OS counts them, instructions an operation, the bench being one thread. It also reads the
//! allocations each side made (`hyper_measure::alloc`, with the in-memory network's own copies set
//! aside), and the datagrams that crossed either way.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cognitive_complexity,
    missing_docs
)]

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::time::Instant;

use bytes::BytesMut;
use hyper_measure::alloc::{self, Counting, Counts};
use hyper_measure::quantile::{self, Level};

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Shape: the intervals' coverage (`docs/tails.md` §3.3).
const COVERAGE: Level = match Level::new(95, 100) {
    Ok(level) => level,
    Err(_) => Level::P50,
};

/// Shape: what a writer hands its stream at a time, and the largest exchange.
static CHUNK: [u8; 65_536] = [0x5a; 65_536];

/// Shape: the bytes a bulk round moves, 64 MiB.
const BULK_BYTES: u64 = 64 << 20;

/// Shape: the exchange sizes, each way: a small request (focal's votes and acknowledgements), a page,
/// and the largest a writer hands at once here.
const EXCHANGE_SIZES: [usize; 3] = [64, 4_096, 65_536];

/// Shape: exchanges of each size a side a round: a round's thousands of datagrams, long against its
/// clock reads.
const EXCHANGES: usize = 2_000;

/// Bench budget: the driver's steps one operation may take before the run is refused as stuck.
const STEPS: usize = 1 << 24;

const CLIENT: SocketAddr = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 4433);
const SERVER: SocketAddr = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 4434);

/// A QUIC endpoint holding one connection at a time, either implementation.
trait Quic {
    type Stream: Copy;
    /// Starts the client's connection to `server`.
    fn connect(&mut self, now: Instant, server: SocketAddr);
    /// Takes one datagram; the size of a response the endpoint sends at once, written to `out`.
    fn datagram(
        &mut self,
        now: Instant,
        from: SocketAddr,
        data: BytesMut,
        out: &mut Vec<u8>,
    ) -> Option<usize>;
    /// The size of the next datagram to send, written to `out`.
    fn transmit(&mut self, now: Instant, out: &mut Vec<u8>) -> Option<usize>;
    fn timeout(&mut self) -> Option<Instant>;
    fn fire(&mut self, now: Instant);
    /// Takes the connection's events; a drained connection is let go, its endpoint told.
    fn events(&mut self);
    /// Whether the connection has said it is connected.
    fn connected(&self) -> bool;
    fn open(&mut self) -> Option<Self::Stream>;
    fn accept(&mut self) -> Option<Self::Stream>;
    /// Writes as much of `data` as the stream takes now.
    fn write(&mut self, stream: Self::Stream, data: &[u8]) -> usize;
    fn finish(&mut self, stream: Self::Stream);
    /// The bytes the stream had to read, read and dropped; whether it has finished.
    fn read(&mut self, stream: Self::Stream) -> (u64, bool);
    fn close(&mut self, now: Instant);
    /// Whether the endpoint still holds a connection that has not drained.
    fn live(&self) -> bool;
}

/// A certificate for "localhost" and its key, DER.
fn certificate() -> (Vec<u8>, Vec<u8>) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    (cert.cert.der().to_vec(), cert.signing_key.serialize_der())
}

/// hyper-quic, its configurations owned by its endpoint.
mod ours {
    use std::sync::LazyLock;

    use super::{BytesMut, Instant, Quic, SocketAddr};
    use hyper_quic::TransportConfig;
    use hyper_quic::crypto::rustls::{QuicClientConfig, QuicServerConfig};
    use hyper_quic::rustls::client::Resumption;
    use hyper_quic::rustls::crypto::{CryptoProvider, aws_lc_rs};
    use hyper_quic::rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
    use hyper_quic::rustls::{self, RootCertStore};
    use hyper_quic::{
        ClientConfig, ClientConfigHandle, Connection, ConnectionHandle, DatagramEvent, Dir,
        Endpoint, EndpointConfig, Event, ServerConfig, StreamId, VarInt,
    };

    static PROVIDER: LazyLock<CryptoProvider> = LazyLock::new(|| CryptoProvider {
        kx_groups: vec![aws_lc_rs::kx_group::X25519MLKEM768],
        cipher_suites: vec![
            aws_lc_rs::cipher_suite::TLS13_AES_256_GCM_SHA384,
            aws_lc_rs::cipher_suite::TLS13_CHACHA20_POLY1305_SHA256,
        ],
        ..aws_lc_rs::default_provider()
    });

    pub(super) struct Side {
        endpoint: Endpoint,
        client: Option<ClientConfigHandle>,
        conn: Option<(ConnectionHandle, Connection)>,
        connected: bool,
    }

    /// The endpoint's configuration: the default, or without Careful Resume (`plain`).
    fn endpoint_config(plain: bool) -> EndpointConfig {
        let mut config = EndpointConfig::default();
        if plain {
            config.careful_resume(None);
        }
        config
    }

    /// The transport's configuration: the default, or without the handshake's copies (`plain`).
    fn transport(plain: bool) -> TransportConfig {
        let mut transport = TransportConfig::default();
        if plain {
            transport.handshake_copies(false);
        }
        transport
    }

    pub(super) fn server(certificate: &[u8], key: &[u8], plain: bool) -> Side {
        let mut tls = rustls::ServerConfig::builder_with_provider(&PROVIDER)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(certificate.to_vec())],
                PrivatePkcs8KeyDer::from(key.to_vec()).into(),
            )
            .unwrap();
        tls.max_early_data_size = u32::MAX;
        let mut config =
            ServerConfig::with_crypto(Box::new(QuicServerConfig::try_from(tls).unwrap()));
        config.transport_config(transport(plain));
        Side {
            endpoint: Endpoint::new(endpoint_config(plain), Some(config), true, None).unwrap(),
            client: None,
            conn: None,
            connected: false,
        }
    }

    pub(super) fn client(certificate: &[u8], resumes: bool, plain: bool) -> Side {
        let mut endpoint = Endpoint::new(endpoint_config(plain), None, true, None).unwrap();
        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from(certificate.to_vec()))
            .unwrap();
        let mut tls = rustls::ClientConfig::builder_with_provider(&PROVIDER)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        tls.enable_early_data = true;
        if !resumes {
            tls.resumption = Resumption::disabled();
        }
        let mut config = ClientConfig::new(Box::new(QuicClientConfig::try_from(tls).unwrap()));
        config.transport_config(transport(plain));
        let client = endpoint.insert_client_config(config).unwrap();
        Side {
            endpoint,
            client: Some(client),
            conn: None,
            connected: false,
        }
    }

    impl Quic for Side {
        type Stream = StreamId;
        fn connect(&mut self, now: Instant, server: SocketAddr) {
            let client = self.client.unwrap();
            self.conn = Some(
                self.endpoint
                    .connect(now, client, server, "localhost", None)
                    .unwrap(),
            );
            self.connected = false;
        }
        fn datagram(
            &mut self,
            now: Instant,
            from: SocketAddr,
            data: BytesMut,
            out: &mut Vec<u8>,
        ) -> Option<usize> {
            out.clear();
            match self.endpoint.handle(now, from, None, None, data, out) {
                Some(DatagramEvent::NewConnection(incoming)) => {
                    let conn = self
                        .endpoint
                        .accept(incoming, now, out, None, None)
                        .unwrap();
                    self.conn = Some(conn);
                    self.connected = false;
                    None
                }
                Some(DatagramEvent::ConnectionEvent(handle, event)) => {
                    if let Some((own, conn)) = &mut self.conn
                        && *own == handle
                    {
                        conn.handle_event(event, self.endpoint.configs_mut());
                    }
                    None
                }
                Some(DatagramEvent::Response(t)) => Some(t.size),
                None => None,
            }
        }
        fn transmit(&mut self, now: Instant, out: &mut Vec<u8>) -> Option<usize> {
            let (handle, conn) = self.conn.as_mut()?;
            while let Some(event) = conn.poll_endpoint_events() {
                if let Some(event) = self.endpoint.handle_event(*handle, event) {
                    conn.handle_event(event, self.endpoint.configs_mut());
                }
            }
            out.clear();
            conn.poll_transmit(now, 1, out, self.endpoint.configs())
                .map(|t| t.size)
        }
        fn timeout(&mut self) -> Option<Instant> {
            self.conn.as_mut()?.1.poll_timeout()
        }
        fn fire(&mut self, now: Instant) {
            if let Some((_, conn)) = &mut self.conn {
                conn.handle_timeout(now);
            }
        }
        fn events(&mut self) {
            let Some((handle, conn)) = &mut self.conn else {
                return;
            };
            while let Some(event) = conn.poll() {
                if matches!(event, Event::Connected) {
                    self.connected = true;
                }
            }
            if conn.is_drained() {
                while let Some(event) = conn.poll_endpoint_events() {
                    let _ = self.endpoint.handle_event(*handle, event);
                }
                self.conn = None;
                self.connected = false;
            }
        }
        fn connected(&self) -> bool {
            self.connected
        }
        fn open(&mut self) -> Option<StreamId> {
            self.conn.as_mut()?.1.streams().open(Dir::Bi)
        }
        fn accept(&mut self) -> Option<StreamId> {
            self.conn.as_mut()?.1.streams().accept(Dir::Bi)
        }
        fn write(&mut self, stream: StreamId, data: &[u8]) -> usize {
            let Some((_, conn)) = &mut self.conn else {
                return 0;
            };
            conn.send_stream(stream).write(data).unwrap_or(0)
        }
        fn finish(&mut self, stream: StreamId) {
            if let Some((_, conn)) = &mut self.conn {
                conn.send_stream(stream).finish().unwrap();
            }
        }
        fn read(&mut self, stream: StreamId) -> (u64, bool) {
            let Some((_, conn)) = &mut self.conn else {
                return (0, false);
            };
            let mut recv = conn.recv_stream(stream);
            let Ok(mut chunks) = recv.read(true) else {
                return (0, false);
            };
            let mut got = 0;
            let mut finished = false;
            loop {
                match chunks.next(usize::MAX) {
                    Ok(Some(chunk)) => got += chunk.bytes.len() as u64,
                    Ok(None) => {
                        finished = true;
                        break;
                    }
                    Err(_) => break,
                }
            }
            let _ = chunks.finalize();
            (got, finished)
        }
        fn close(&mut self, now: Instant) {
            if let Some((_, conn)) = &mut self.conn {
                conn.close(now, VarInt::from_u32(0), bytes::Bytes::new());
            }
        }
        fn live(&self) -> bool {
            self.conn.is_some()
        }
    }
}

/// Upstream quinn-proto: the same, its configurations shared by `Arc`.
mod theirs {
    use std::sync::{Arc, LazyLock};

    use super::{BytesMut, Instant, Quic, SocketAddr};
    use upstream_quinn_proto::crypto::rustls::{QuicClientConfig, QuicServerConfig};
    use upstream_quinn_proto::{
        ClientConfig, Connection, ConnectionHandle, DatagramEvent, Dir, Endpoint, EndpointConfig,
        Event, ServerConfig, StreamId, VarInt,
    };
    use upstream_rustls::client::Resumption;
    use upstream_rustls::crypto::{CryptoProvider, aws_lc_rs};
    use upstream_rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
    use upstream_rustls::quic::Suite;
    use upstream_rustls::{self as rustls, RootCertStore};

    static PROVIDER: LazyLock<Arc<CryptoProvider>> = LazyLock::new(|| {
        Arc::new(CryptoProvider {
            kx_groups: vec![aws_lc_rs::kx_group::X25519MLKEM768],
            cipher_suites: vec![
                aws_lc_rs::cipher_suite::TLS13_AES_256_GCM_SHA384,
                aws_lc_rs::cipher_suite::TLS13_CHACHA20_POLY1305_SHA256,
            ],
            ..aws_lc_rs::default_provider()
        })
    });

    /// Initial packets' suite, AES-128-GCM (RFC 9001 §5.2), which the provider above does not
    /// offer for the connection: hyper-quic takes it from aws-lc-rs whatever the provider offers.
    fn initial() -> Suite {
        aws_lc_rs::cipher_suite::TLS13_AES_128_GCM_SHA256
            .tls13()
            .unwrap()
            .quic_suite()
            .unwrap()
    }

    pub(super) struct Side {
        endpoint: Endpoint,
        client: Option<ClientConfig>,
        conn: Option<(ConnectionHandle, Connection)>,
        connected: bool,
    }

    pub(super) fn server(certificate: &[u8], key: &[u8]) -> Side {
        let mut tls = rustls::ServerConfig::builder_with_provider(PROVIDER.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(certificate.to_vec())],
                PrivatePkcs8KeyDer::from(key.to_vec()).into(),
            )
            .unwrap();
        tls.max_early_data_size = u32::MAX;
        let crypto = QuicServerConfig::with_initial(Arc::new(tls), initial()).unwrap();
        let config = ServerConfig::with_crypto(Arc::new(crypto));
        Side {
            endpoint: Endpoint::new(
                Arc::new(EndpointConfig::default()),
                Some(Arc::new(config)),
                true,
                None,
            ),
            client: None,
            conn: None,
            connected: false,
        }
    }

    pub(super) fn client(certificate: &[u8], resumes: bool) -> Side {
        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from(certificate.to_vec()))
            .unwrap();
        let mut tls = rustls::ClientConfig::builder_with_provider(PROVIDER.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        tls.enable_early_data = true;
        if !resumes {
            tls.resumption = Resumption::disabled();
        }
        let crypto = QuicClientConfig::with_initial(Arc::new(tls), initial()).unwrap();
        Side {
            endpoint: Endpoint::new(Arc::new(EndpointConfig::default()), None, true, None),
            client: Some(ClientConfig::new(Arc::new(crypto))),
            conn: None,
            connected: false,
        }
    }

    impl Quic for Side {
        type Stream = StreamId;
        fn connect(&mut self, now: Instant, server: SocketAddr) {
            let client = self.client.clone().unwrap();
            self.conn = Some(
                self.endpoint
                    .connect(now, client, server, "localhost")
                    .unwrap(),
            );
            self.connected = false;
        }
        fn datagram(
            &mut self,
            now: Instant,
            from: SocketAddr,
            data: BytesMut,
            out: &mut Vec<u8>,
        ) -> Option<usize> {
            out.clear();
            match self.endpoint.handle(now, from, None, None, data, out) {
                Some(DatagramEvent::NewConnection(incoming)) => {
                    let conn = self.endpoint.accept(incoming, now, out, None).unwrap();
                    self.conn = Some(conn);
                    self.connected = false;
                    None
                }
                Some(DatagramEvent::ConnectionEvent(handle, event)) => {
                    if let Some((own, conn)) = &mut self.conn
                        && *own == handle
                    {
                        conn.handle_event(event);
                    }
                    None
                }
                Some(DatagramEvent::Response(t)) => Some(t.size),
                None => None,
            }
        }
        fn transmit(&mut self, now: Instant, out: &mut Vec<u8>) -> Option<usize> {
            let (handle, conn) = self.conn.as_mut()?;
            while let Some(event) = conn.poll_endpoint_events() {
                if let Some(event) = self.endpoint.handle_event(*handle, event) {
                    conn.handle_event(event);
                }
            }
            out.clear();
            conn.poll_transmit(now, 1, out).map(|t| t.size)
        }
        fn timeout(&mut self) -> Option<Instant> {
            self.conn.as_mut()?.1.poll_timeout()
        }
        fn fire(&mut self, now: Instant) {
            if let Some((_, conn)) = &mut self.conn {
                conn.handle_timeout(now);
            }
        }
        fn events(&mut self) {
            let Some((handle, conn)) = &mut self.conn else {
                return;
            };
            while let Some(event) = conn.poll() {
                if matches!(event, Event::Connected) {
                    self.connected = true;
                }
            }
            if conn.is_drained() {
                while let Some(event) = conn.poll_endpoint_events() {
                    let _ = self.endpoint.handle_event(*handle, event);
                }
                self.conn = None;
                self.connected = false;
            }
        }
        fn connected(&self) -> bool {
            self.connected
        }
        fn open(&mut self) -> Option<StreamId> {
            self.conn.as_mut()?.1.streams().open(Dir::Bi)
        }
        fn accept(&mut self) -> Option<StreamId> {
            self.conn.as_mut()?.1.streams().accept(Dir::Bi)
        }
        fn write(&mut self, stream: StreamId, data: &[u8]) -> usize {
            let Some((_, conn)) = &mut self.conn else {
                return 0;
            };
            conn.send_stream(stream).write(data).unwrap_or(0)
        }
        fn finish(&mut self, stream: StreamId) {
            if let Some((_, conn)) = &mut self.conn {
                conn.send_stream(stream).finish().unwrap();
            }
        }
        fn read(&mut self, stream: StreamId) -> (u64, bool) {
            let Some((_, conn)) = &mut self.conn else {
                return (0, false);
            };
            let mut recv = conn.recv_stream(stream);
            let Ok(mut chunks) = recv.read(true) else {
                return (0, false);
            };
            let mut got = 0;
            let mut finished = false;
            loop {
                match chunks.next(usize::MAX) {
                    Ok(Some(chunk)) => got += chunk.bytes.len() as u64,
                    Ok(None) => {
                        finished = true;
                        break;
                    }
                    Err(_) => break,
                }
            }
            let _ = chunks.finalize();
            (got, finished)
        }
        fn close(&mut self, now: Instant) {
            if let Some((_, conn)) = &mut self.conn {
                conn.close(now, VarInt::from_u32(0), bytes::Bytes::new());
            }
        }
        fn live(&self) -> bool {
            self.conn.is_some()
        }
    }
}

/// A datagram into the in-memory network: the network's copy, not either side's, so set aside.
fn carry(queue: &mut VecDeque<BytesMut>, datagram: &[u8]) {
    alloc::aside();
    queue.push_back(BytesMut::from(datagram));
    alloc::back();
}

/// A client and a server over memory on a virtual clock.
struct Net<Q: Quic> {
    now: Instant,
    client: Q,
    server: Q,
    to_server: VecDeque<BytesMut>,
    to_client: VecDeque<BytesMut>,
    out: Vec<u8>,
    datagrams: u64,
}

impl<Q: Quic> Net<Q> {
    fn new(client: Q, server: Q) -> Self {
        Self {
            now: hyper_sim::Anchor::new().instant(0).unwrap(),
            client,
            server,
            to_server: VecDeque::with_capacity(1_024),
            to_client: VecDeque::with_capacity(1_024),
            out: Vec::with_capacity(65_536),
            datagrams: 0,
        }
    }

    /// Moves every datagram either side sends until, at this time, neither sends nor receives any;
    /// whether any moved.
    fn exchange(&mut self) -> bool {
        let mut moved = false;
        loop {
            let mut any = false;
            while let Some(size) = self.client.transmit(self.now, &mut self.out) {
                carry(&mut self.to_server, &self.out[..size]);
                any = true;
            }
            while let Some(size) = self.server.transmit(self.now, &mut self.out) {
                carry(&mut self.to_client, &self.out[..size]);
                any = true;
            }
            while let Some(datagram) = self.to_server.pop_front() {
                self.datagrams += 1;
                any = true;
                if let Some(size) = self
                    .server
                    .datagram(self.now, CLIENT, datagram, &mut self.out)
                {
                    carry(&mut self.to_client, &self.out[..size]);
                }
            }
            while let Some(datagram) = self.to_client.pop_front() {
                self.datagrams += 1;
                any = true;
                if let Some(size) = self
                    .client
                    .datagram(self.now, SERVER, datagram, &mut self.out)
                {
                    carry(&mut self.to_server, &self.out[..size]);
                }
            }
            self.client.events();
            self.server.events();
            if !any {
                return moved;
            }
            moved = true;
        }
    }

    /// Moves the clock to the earliest timer and fires what is due; whether there was one.
    fn advance(&mut self) -> bool {
        let Some(next) = [self.client.timeout(), self.server.timeout()]
            .into_iter()
            .flatten()
            .min()
        else {
            return false;
        };
        self.now = self.now.max(next);
        self.client.fire(self.now);
        self.server.fire(self.now);
        true
    }

    /// Connects, and returns once both sides have connected and nothing moves.
    fn handshake(&mut self) {
        self.client.connect(self.now, SERVER);
        for _ in 0..STEPS {
            self.exchange();
            if self.client.connected() && self.server.connected() {
                return;
            }
            assert!(self.advance(), "a handshake stalled with no timer");
        }
        panic!("no handshake within the bench's budget");
    }

    /// The client closes; returns once both sides have drained.
    fn close(&mut self) {
        self.client.close(self.now);
        for _ in 0..STEPS {
            self.exchange();
            if !self.client.live() && !self.server.live() {
                return;
            }
            assert!(self.advance(), "a closing connection stalled with no timer");
        }
        panic!("no drain within the bench's budget");
    }

    /// A bidirectional stream of the client's, once the peer's credit allows one.
    fn open(&mut self) -> Q::Stream {
        for _ in 0..STEPS {
            if let Some(stream) = self.client.open() {
                return stream;
            }
            if !self.exchange() {
                assert!(self.advance(), "no stream credit and no timer");
            }
        }
        panic!("no stream within the bench's budget");
    }

    /// One exchange of `size` bytes each way on a stream of its own.
    fn exchange_once(&mut self, size: usize) {
        let stream = self.open();
        let (mut sent, mut request, mut replied, mut reply) = (0, 0, 0, 0);
        let mut served = None;
        let mut requested = false;
        for _ in 0..STEPS {
            let mut progressed = false;
            if sent < size {
                let n = self.client.write(stream, &CHUNK[..size - sent]);
                sent += n;
                progressed |= n > 0;
                if sent == size {
                    self.client.finish(stream);
                }
            }
            progressed |= self.exchange();
            if served.is_none() {
                served = self.server.accept();
            }
            if let Some(s) = served {
                if !requested {
                    let (n, finished) = self.server.read(s);
                    request += n;
                    progressed |= n > 0;
                    requested = finished;
                }
                if requested && replied < size {
                    let n = self.server.write(s, &CHUNK[..size - replied]);
                    replied += n;
                    progressed |= n > 0;
                    if replied == size {
                        self.server.finish(s);
                    }
                }
            }
            let (n, finished) = self.client.read(stream);
            reply += n;
            progressed |= n > 0;
            if finished {
                assert_eq!((request, reply), (size as u64, size as u64));
                return;
            }
            if !progressed {
                assert!(self.advance(), "an exchange stalled with no timer");
            }
        }
        panic!("no exchange within the bench's budget");
    }

    /// `bytes` client to server on one stream.
    fn bulk(&mut self, bytes: u64) {
        let stream = self.open();
        let (mut sent, mut got, mut finished) = (0u64, 0u64, false);
        let mut served = None;
        for _ in 0..STEPS {
            let mut progressed = false;
            while sent < bytes {
                let take = (bytes - sent).min(CHUNK.len() as u64) as usize;
                let n = self.client.write(stream, &CHUNK[..take]);
                if n == 0 {
                    break;
                }
                sent += n as u64;
                progressed = true;
            }
            if sent == bytes && !finished {
                self.client.finish(stream);
                finished = true;
            }
            progressed |= self.exchange();
            if served.is_none() {
                served = self.server.accept();
            }
            if let Some(s) = served {
                let (n, done) = self.server.read(s);
                got += n;
                progressed |= n > 0;
                if done {
                    assert_eq!(got, bytes);
                    return;
                }
            }
            if !progressed {
                assert!(self.advance(), "the transfer stalled with no timer");
            }
        }
        panic!("no transfer within the bench's budget");
    }
}

/// Which implementation a run is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    /// hyper-quic with its defaults.
    Ours,
    /// hyper-quic with the handshake's copies and Careful Resume off: upstream's wire.
    Plain,
    Theirs,
}

impl Side {
    const ALL: [Self; 3] = [Self::Ours, Self::Plain, Self::Theirs];

    fn name(self) -> &'static str {
        match self {
            Self::Ours => "hyper-quic",
            Self::Plain => "hyper-quic, upstream's wire",
            Self::Theirs => "upstream",
        }
    }
}

/// A workload a row of the table states.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Work {
    Full,
    Resumed,
    Exchange(usize),
    Bulk,
}

impl Work {
    fn all() -> Vec<Self> {
        let mut all = vec![Self::Full, Self::Resumed];
        all.extend(EXCHANGE_SIZES.iter().map(|&size| Self::Exchange(size)));
        all.push(Self::Bulk);
        all
    }

    fn name(self) -> String {
        match self {
            Self::Full => "handshake, full".to_owned(),
            Self::Resumed => "handshake, resumed by ticket".to_owned(),
            Self::Exchange(size) if size >= 1_024 => {
                format!("exchange, {} KiB each way", size >> 10)
            }
            Self::Exchange(size) => format!("exchange, {size} B each way"),
            Self::Bulk => "bulk, a MiB".to_owned(),
        }
    }
}

/// What the process's account and the allocator charged a workload, and what crossed.
#[derive(Clone, Copy, Debug, Default)]
struct Account {
    cpu_ns: u64,
    instructions: u64,
    counted: bool,
    allocations: u64,
    reallocations: u64,
    bytes: u64,
    datagrams: u64,
    /// The operations (or MiB) the account covers.
    ops: u64,
}

impl Account {
    fn add(&mut self, other: Self) {
        self.cpu_ns += other.cpu_ns;
        self.instructions += other.instructions;
        self.counted |= other.counted;
        self.allocations += other.allocations;
        self.reallocations += other.reallocations;
        self.bytes += other.bytes;
        self.datagrams += other.datagrams;
        self.ops += other.ops;
    }

    fn per(&self, total: u64) -> f64 {
        total as f64 / self.ops.max(1) as f64
    }
}

/// Runs `f` over `net`, charging it the process's CPU and instructions, the allocations made outside
/// the network's copies, and the datagrams that crossed, as `ops` operations.
fn accounted<Q: Quic>(net: &mut Net<Q>, ops: u64, f: impl FnOnce(&mut Net<Q>)) -> Account {
    let datagrams = net.datagrams;
    let before = hyper_measure::usage::this().unwrap();
    alloc::begin();
    f(net);
    let total = alloc::end();
    let spent = hyper_measure::usage::this().unwrap().since(&before);
    let own: Counts = total.less(&alloc::read_aside());
    Account {
        cpu_ns: spent.user_ns + spent.system_ns,
        instructions: spent.instructions.unwrap_or(0),
        counted: spent.instructions.is_some(),
        allocations: own.allocations,
        reallocations: own.reallocations,
        bytes: own.bytes,
        datagrams: net.datagrams - datagrams,
        ops,
    }
}

/// One side's turn at one workload: its timed operations appended to `times` (handshakes and
/// exchanges, each on its own; a bulk round's nanoseconds a MiB), and its account.
fn run<Q: Quic>(
    work: Work,
    count: usize,
    client: impl Fn(bool) -> Q,
    server: impl Fn() -> Q,
    times: &mut Vec<u64>,
) -> Account {
    let resumes = work == Work::Resumed;
    let mut net = Net::new(client(resumes), server());
    // A first connection: it warms both endpoints and, resuming, leaves the ticket the first timed
    // handshake resumes from.
    net.handshake();
    match work {
        Work::Full | Work::Resumed => {
            net.close();
            times.reserve(count);
            accounted(&mut net, count as u64, |net| {
                for _ in 0..count {
                    let began = Instant::now();
                    net.handshake();
                    times.push(began.elapsed().as_nanos() as u64);
                    net.close();
                }
            })
        }
        Work::Exchange(size) => {
            times.reserve(count);
            let account = accounted(&mut net, count as u64, |net| {
                for _ in 0..count {
                    let began = Instant::now();
                    net.exchange_once(size);
                    times.push(began.elapsed().as_nanos() as u64);
                }
            });
            net.close();
            account
        }
        Work::Bulk => {
            let mut elapsed = 0;
            let account = accounted(&mut net, BULK_BYTES >> 20, |net| {
                let began = Instant::now();
                net.bulk(BULK_BYTES);
                elapsed = began.elapsed().as_nanos() as u64;
            });
            times.push(elapsed / (BULK_BYTES >> 20));
            net.close();
            account
        }
    }
}

fn turn(
    side: Side,
    work: Work,
    count: usize,
    cert: &(Vec<u8>, Vec<u8>),
    times: &mut Vec<u64>,
) -> Account {
    let (certificate, key) = (cert.0.as_slice(), cert.1.as_slice());
    match side {
        Side::Ours | Side::Plain => {
            let plain = side == Side::Plain;
            run(
                work,
                count,
                |resumes| ours::client(certificate, resumes, plain),
                || ours::server(certificate, key, plain),
                times,
            )
        }
        Side::Theirs => run(
            work,
            count,
            |resumes| theirs::client(certificate, resumes),
            || theirs::server(certificate, key),
            times,
        ),
    }
}

fn main() {
    let rounds: usize = std::env::args()
        .skip(1)
        .find(|a| !a.starts_with('-'))
        .and_then(|a| a.parse().ok())
        .unwrap_or(5)
        .max(1);
    assert!(
        alloc::installed(),
        "the counting allocator is not installed"
    );
    let handshakes = quantile::samples_for(Level::P99, COVERAGE).unwrap() as usize;
    let per_round = handshakes.div_ceil(rounds);
    println!(
        "hyper-quic against upstream quinn-proto 0.11.18: {rounds} rounds, {} handshakes of each shape and {} exchanges of each size an arm",
        per_round * rounds,
        EXCHANGES * rounds
    );
    let cert = certificate();
    let works = Work::all();
    // Per arm and workload: the timed operations and the account.
    let mut results: Vec<(Side, Work, Vec<u64>, Account)> = Vec::new();
    for side in Side::ALL {
        for &work in &works {
            results.push((side, work, Vec::new(), Account::default()));
        }
    }
    // The process's one-time costs first (the cryptography library's start), once in each arm.
    let mut discard = Vec::new();
    for side in Side::ALL {
        let _ = turn(side, Work::Full, 1, &cert, &mut discard);
    }
    for round in 0..rounds {
        for at in 0..Side::ALL.len() {
            let side = Side::ALL[(round + at) % Side::ALL.len()];
            for (s, work, times, account) in &mut results {
                if *s != side {
                    continue;
                }
                let count = match work {
                    Work::Full | Work::Resumed => per_round,
                    Work::Exchange(_) => EXCHANGES,
                    Work::Bulk => 1,
                };
                account.add(turn(side, *work, count, &cert, times));
            }
        }
    }
    let us = |ns: u64| format!("{:.1}", ns as f64 / 1e3);
    println!();
    println!(
        "| workload | arm | p50 [95%] µs | p99 [95%] µs | CPU µs | instructions | vs upstream: p50, CPU, instructions | allocations | reallocations | datagrams |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|");
    for &work in &works {
        let pick = |side: Side| -> (Vec<u64>, Account) {
            let (_, _, times, account) = results
                .iter()
                .find(|(s, w, _, _)| *s == side && *w == work)
                .unwrap();
            let mut times = times.clone();
            times.sort_unstable();
            (times, *account)
        };
        let p50 = |v: &[u64]| {
            quantile::quantile(v, Level::P50, COVERAGE)
                .unwrap()
                .estimate as f64
        };
        let cpu = |a: &Account| a.per(a.cpu_ns) / 1e3;
        let (theirs, theirs_account) = pick(Side::Theirs);
        for side in Side::ALL {
            let (times, account) = pick(side);
            let p99 = if work == Work::Bulk {
                "one a round".to_owned()
            } else {
                quantile::cell(&times, Level::P99, COVERAGE, us)
            };
            let (instructions, ratio) = if account.counted {
                let own = account.per(account.instructions);
                (
                    format!("{own:.0}"),
                    format!(
                        "{:.3}",
                        own / theirs_account.per(theirs_account.instructions)
                    ),
                )
            } else {
                ("not counted here".to_owned(), "—".to_owned())
            };
            println!(
                "| {} | {} | {} | {p99} | {:.1} | {instructions} | {:.3}, {:.3}, {ratio} | {:.1} | {:.2} | {:.1} |",
                work.name(),
                side.name(),
                quantile::cell(&times, Level::P50, COVERAGE, us),
                cpu(&account),
                p50(&times) / p50(&theirs),
                cpu(&account) / cpu(&theirs_account),
                account.per(account.allocations),
                account.per(account.reallocations),
                account.per(account.datagrams),
            );
        }
    }
}
