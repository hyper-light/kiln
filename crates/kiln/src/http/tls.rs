//! TLS on hyper-tls (hyper-raft's rustls fork, reached as `hyper_quic::rustls`), driven over any
//! futures-io transport, for both ends: HTTPS clients ([`ClientTls`], [`TlsStream`]) and servers
//! ([`ServerTls`], [`ServerTlsStream`]: the server's own listener with `server.https`, and a
//! runner's front door). No tokio, no tokio-rustls.
//!
//! hyper-tls connections hold no configuration: every call that may advance the state machine
//! (`process_new_packets(&mut config)`) borrows it, for the connection's whole life (a TLS 1.3
//! client files NewSessionTickets into it after the handshake). So each connection owns its own
//! configuration, built from shared material: a root store ([`ClientTls::config`]), or a
//! certificate and its key, parsed once, and the listener's TLS sessions ([`ServerTls::config`]).
//!
//! A server's sessions outlive the connection that made them (goal 7 step 3, plan
//! 01a1a832ebbf7327a203df446e16ecac): every connection's configuration files the tickets it
//! sends into one bounded store its [`ServerTls`] holds ([`Sessions`]), so a browser coming
//! back resumes, without the certificate and its signature, rather than starting again.

use std::future::poll_fn;
use std::io::{self, BufRead as _, Read as _, Write as _};
use std::collections::{BTreeMap, HashMap};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use futures::io::{AsyncRead, AsyncWrite};
use hyper_quic::rustls::crypto::CryptoProvider;
use hyper_quic::rustls::pki_types::pem::PemObject as _;
use hyper_quic::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use hyper_quic::rustls::server::{ClientHello, ResolvesServerCert, StoresServerSessions};
use hyper_quic::rustls::sign::CertifiedKey;
use hyper_quic::rustls::{self, ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection};

use super::client::{Identity, Roots};

/// Providers for HTTPS to arbitrary servers: aws-lc-rs's defaults (TLS 1.3 and 1.2, X25519MLKEM768,
/// X25519, P-256, P-384). Not hyper-quic's node provider, which is post-quantum only and is refused
/// by classical servers such as GitHub (and by browsers that offer no post-quantum group).
fn provider() -> &'static CryptoProvider {
    &rustls::crypto::aws_lc_rs::DEFAULT_PROVIDER
}

/// Trust and identity for a client, from which each connection's configuration is made.
pub struct ClientTls {
    roots: RootCertStore,
    identity: Option<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)>,
}

impl ClientTls {
    pub fn new(roots: Roots, identity: Option<Identity>) -> anyhow::Result<Self> {
        let mut store = RootCertStore::empty();
        match roots {
            // The public roots, and this machine's own (a TLS-inspecting proxy's root, say).
            Roots::WebPki => {
                store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
                store.add_parsable_certificates(super::roots::system().iter().cloned());
            }
            Roots::Pem(pem) => {
                for cert in CertificateDer::pem_slice_iter(&pem) {
                    store.add(cert.context("CA PEM")?).context("CA certificate")?;
                }
            }
            Roots::WebPkiPlusDer(der) => {
                store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
                store.add_parsable_certificates(super::roots::system().iter().cloned());
                store.add(CertificateDer::from(der)).context("CA certificate")?;
            }
        }
        anyhow::ensure!(!store.is_empty(), "no usable root certificates");
        let identity = match identity {
            None => None,
            Some(id) => {
                let chain = CertificateDer::pem_slice_iter(&id.cert_pem)
                    .collect::<Result<Vec<_>, _>>()
                    .context("client certificate PEM")?;
                anyhow::ensure!(!chain.is_empty(), "no CERTIFICATE in the client certificate PEM");
                let key = PrivateKeyDer::from_pem_slice(&id.key_pem).context("client key PEM")?;
                Some((chain, key))
            }
        };
        let tls = Self { roots: store, identity };
        // Fail now, not at the first request, for a key the provider cannot load.
        tls.config()?;
        Ok(tls)
    }

    /// A fresh configuration for one connection (TLS 1.3 and 1.2, ALPN http/1.1).
    pub fn config(&self) -> anyhow::Result<ClientConfig> {
        self.config_alpn(&[HTTP11])
    }

    /// [`Self::config`] asking for these protocols (ALPN), in order of preference.
    pub fn config_alpn(&self, alpn: &[&[u8]]) -> anyhow::Result<ClientConfig> {
        let builder = ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .context("protocol versions")?
            .with_root_certificates(self.roots.clone());
        let mut config = match &self.identity {
            Some((chain, key)) => builder
                .with_client_auth_cert(chain.clone(), key.clone_key())
                .context("client certificate and key")?,
            None => builder.with_no_client_auth(),
        };
        config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
        Ok(config)
    }
}

/// The TLS name for a URL host: a DNS name is sent as SNI; an IP literal is matched against IP SANs
/// and never sent as SNI. IPv6 brackets are stripped.
pub fn server_name(host: &str) -> anyhow::Result<ServerName<'static>> {
    let bare = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    ServerName::try_from(bare.to_owned()).map_err(|_| anyhow::anyhow!("invalid TLS server name {host:?}"))
}

/// ALPN's name for HTTP/2.
pub const H2: &[u8] = b"h2";
/// ALPN's name for HTTP/1.1.
pub const HTTP11: &[u8] = b"http/1.1";

// ------------------------------------------------------------------------------- server side

/// How a TLS listener keeps sessions for browsers to resume (goal 7 step 3): stateful, each
/// TLS 1.3 session used once, in one store per listener.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resumption {
    /// Sessions kept at most; past it, the oldest goes first.
    pub capacity: usize,
    /// How long a session may be resumed after it was made, at most [`STATEFUL_LIFETIME`].
    pub lifetime: Duration,
    /// TLS 1.3 tickets sent after each handshake (each resumes once).
    pub tickets: usize,
}

/// The lifetime hyper-tls tells clients a stateful ticket has (24 hours, under TLS 1.3's 7 days):
/// a client drops its ticket then, so a longer [`Resumption::lifetime`] could never be used.
pub const STATEFUL_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);

/// The most a stored session takes (its key and its value): hyper-tls's own encoding of a
/// session without client certificates is a few hundred bytes; one larger is not kept.
pub const SESSION_BYTES: usize = 2048;

impl Default for Resumption {
    /// 4,096 sessions (at most about 8 MiB, typically a few hundred KiB), resumable for 24 hours,
    /// and hyper-tls's 2 tickets a handshake (as before). The lifetime and the tickets are Ada's to
    /// confirm against the lab's count of connections per page load (goal 7 step 2).
    fn default() -> Self {
        Self { capacity: 4096, lifetime: STATEFUL_LIFETIME, tickets: 2 }
    }
}

/// One listener's TLS sessions, shared by the configurations of all its connections: bounded by
/// [`Resumption::capacity`] (oldest first out) and [`SESSION_BYTES`] each, and a TLS 1.3
/// session is taken out when it is resumed, so a ticket resumes once and is refused after. In
/// memory only: a restart empties it, and browsers' next connects are full handshakes.
pub struct Sessions {
    resumption: Resumption,
    inner: parking_lot::Mutex<Kept>,
}

#[derive(Default)]
struct Kept {
    by_key: HashMap<Vec<u8>, Session>,
    /// Keys in the order they were stored, for oldest-first eviction.
    order: BTreeMap<u64, Vec<u8>>,
    next: u64,
    bytes: usize,
}

struct Session {
    value: Vec<u8>,
    seq: u64,
    made: Instant,
}

impl Kept {
    fn remove(&mut self, key: &[u8]) -> Option<Session> {
        let s = self.by_key.remove(key)?;
        self.order.remove(&s.seq);
        self.bytes = self.bytes.saturating_sub(key.len().saturating_add(s.value.len()));
        Some(s)
    }
}

impl std::fmt::Debug for Sessions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the keys or the secrets.
        f.debug_struct("Sessions").field("resumption", &self.resumption).field("kept", &self.len()).finish()
    }
}

impl Sessions {
    pub fn new(resumption: Resumption) -> Self {
        let resumption = Resumption { lifetime: resumption.lifetime.min(STATEFUL_LIFETIME), ..resumption };
        Self { resumption, inner: parking_lot::Mutex::new(Kept::default()) }
    }

    pub fn resumption(&self) -> Resumption {
        self.resumption
    }

    /// Sessions kept now.
    pub fn len(&self) -> usize {
        self.inner.lock().by_key.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The bytes of the sessions kept now (keys and values).
    pub fn bytes(&self) -> usize {
        self.inner.lock().bytes
    }

    fn fresh(&self, s: &Session) -> bool {
        s.made.elapsed() < self.resumption.lifetime
    }

    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> bool {
        let size = key.len().saturating_add(value.len());
        if self.resumption.capacity == 0 || size > SESSION_BYTES {
            return false;
        }
        let mut kept = self.inner.lock();
        kept.remove(&key);
        while kept.by_key.len() >= self.resumption.capacity {
            let Some((_, oldest)) = kept.order.pop_first() else { break };
            kept.remove(&oldest);
        }
        let seq = kept.next;
        kept.next = kept.next.wrapping_add(1);
        kept.order.insert(seq, key.clone());
        kept.bytes = kept.bytes.saturating_add(size);
        kept.by_key.insert(key, Session { value, seq, made: Instant::now() });
        true
    }

    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        let kept = self.inner.lock();
        kept.by_key.get(key).filter(|s| self.fresh(s)).map(|s| s.value.clone())
    }

    fn take(&self, key: &[u8]) -> Option<Vec<u8>> {
        let taken = self.inner.lock().remove(key)?;
        self.fresh(&taken).then_some(taken.value)
    }
}

/// A connection configuration's handle on its listener's [`Sessions`] (hyper-tls's
/// `StoresServerSessions`, over shared state; hyper-tls itself is unchanged).
#[derive(Debug)]
struct SharedSessions(Arc<Sessions>);

impl StoresServerSessions for SharedSessions {
    fn put(&mut self, key: Vec<u8>, value: Vec<u8>) -> bool {
        self.0.put(key, value)
    }

    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.0.get(key)
    }

    fn take(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        self.0.take(key)
    }

    fn can_cache(&self) -> bool {
        self.0.resumption.capacity > 0
    }
}

/// A server's certificate chain and key, parsed (and checked to belong together) once, and the
/// TLS sessions of the listener it serves ([`Sessions`]); each connection's configuration is made
/// from them ([`Self::config`]). Cheap to clone: clones share the certificate and the sessions.
#[derive(Clone)]
pub struct ServerTls {
    key: Arc<CertifiedKey>,
    /// The leaf's names (its subject alternative names), sorted: which sessions it may resume.
    names: Arc<[String]>,
    sessions: Arc<Sessions>,
}

impl std::fmt::Debug for ServerTls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerTls").field("chain", &self.key.cert.len()).field("key", &"<redacted>").field("sessions", &self.sessions).finish()
    }
}

/// One certificate for every connection, whatever name the client asked for.
#[derive(Debug)]
struct OneCertificate(Arc<CertifiedKey>);

impl ResolvesServerCert for OneCertificate {
    fn resolve(&self, _client_hello: ClientHello<'_>) -> Option<&CertifiedKey> {
        Some(&self.0)
    }
}

impl ServerTls {
    /// A PEM certificate chain (leaf first) and its PEM private key (PKCS#8, PKCS#1 or SEC1).
    /// Refuses a chain without certificates, a key that does not parse, and a key that is not the
    /// leaf's. Errors never quote the key. Its sessions start empty ([`Resumption::default`]).
    pub fn from_pem(chain_pem: &str, key_pem: &str) -> anyhow::Result<Self> {
        let chain = CertificateDer::pem_slice_iter(chain_pem.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| anyhow::anyhow!("the certificate is not PEM (-----BEGIN CERTIFICATE-----)"))?;
        anyhow::ensure!(!chain.is_empty(), "no certificate found (-----BEGIN CERTIFICATE-----)");
        let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
            .map_err(|_| anyhow::anyhow!("no private key found (-----BEGIN PRIVATE KEY-----, RSA or EC PRIVATE KEY)"))?;
        let key = CertifiedKey::from_der(chain, key, provider()).map_err(|e| match e {
            rustls::Error::InconsistentKeys(_) => anyhow::anyhow!("the private key is not the certificate's"),
            other => anyhow::anyhow!("the private key cannot be used: {other}"),
        })?;
        let mut names = key.cert.first().and_then(|leaf| crate::pki::cert_info_der(leaf).ok()).map(|i| i.names).unwrap_or_default();
        names.sort();
        names.dedup();
        let tls = Self { key: Arc::new(key), names: names.into(), sessions: Arc::new(Sessions::new(Resumption::default())) };
        tls.config()?;
        Ok(tls)
    }

    /// The leaf certificate (DER).
    pub fn leaf(&self) -> Option<&[u8]> {
        self.key.cert.first().map(|c| c.as_ref())
    }

    /// The listener's sessions.
    pub fn sessions(&self) -> &Arc<Sessions> {
        &self.sessions
    }

    /// This certificate in place of `old` on the same listener: a renewal (a leaf for the same
    /// names) keeps `old`'s sessions, so browsers resume across it; a certificate for other names
    /// starts with none, and every browser's next connect is a full handshake that sees it.
    pub fn succeeding(self, old: &ServerTls) -> Self {
        if !self.names.is_empty() && self.names == old.names {
            Self { sessions: Arc::clone(&old.sessions), ..self }
        } else {
            self
        }
    }

    /// A fresh configuration for one connection (TLS 1.3 and 1.2, ALPN http/1.1, no client
    /// certificates), filing its sessions in the listener's.
    pub fn config(&self) -> anyhow::Result<ServerConfig> {
        self.config_alpn(&[HTTP11])
    }

    /// [`Self::config`] offering these protocols (ALPN), in order of preference: a client asking
    /// for none of them is refused; one asking for none at all gets HTTP/1.1.
    pub fn config_alpn(&self, alpn: &[&[u8]]) -> anyhow::Result<ServerConfig> {
        let mut config = ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .context("protocol versions")?
            .with_no_client_auth()
            .with_cert_resolver(Box::new(OneCertificate(Arc::clone(&self.key))));
        config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
        // Stateful sessions (no ticketer): each TLS 1.3 session resumes once.
        config.session_storage = Box::new(SharedSessions(Arc::clone(&self.sessions)));
        config.send_tls13_tickets = self.sessions.resumption.tickets;
        Ok(config)
    }
}

// ------------------------------------------------------------------------------- the stream

/// What a TLS stream needs of a connection, client or server: the calls of hyper-tls's
/// `ConnectionCommon`, and the configuration its state machine borrows.
pub trait Side {
    type Config;
    fn read_tls(&mut self, rd: &mut dyn io::Read) -> io::Result<usize>;
    fn write_tls(&mut self, wr: &mut dyn io::Write) -> io::Result<usize>;
    fn process_new_packets(&mut self, config: &mut Self::Config) -> Result<rustls::IoState, rustls::Error>;
    fn reader(&mut self) -> rustls::Reader<'_>;
    fn writer(&mut self) -> rustls::Writer<'_>;
    fn wants_write(&self) -> bool;
    fn is_handshaking(&self) -> bool;
    fn send_close_notify(&mut self);
    fn alpn_protocol(&self) -> Option<&[u8]>;
    fn peer_certificates(&self) -> Option<&[CertificateDer<'static>]>;
    fn handshake_kind(&self) -> Option<rustls::HandshakeKind>;
}

macro_rules! side {
    ($conn:ty, $config:ty) => {
        impl Side for $conn {
            type Config = $config;
            fn read_tls(&mut self, rd: &mut dyn io::Read) -> io::Result<usize> {
                (**self).read_tls(rd)
            }
            fn write_tls(&mut self, wr: &mut dyn io::Write) -> io::Result<usize> {
                (**self).write_tls(wr)
            }
            fn process_new_packets(&mut self, config: &mut $config) -> Result<rustls::IoState, rustls::Error> {
                (**self).process_new_packets(config)
            }
            fn reader(&mut self) -> rustls::Reader<'_> {
                (**self).reader()
            }
            fn writer(&mut self) -> rustls::Writer<'_> {
                (**self).writer()
            }
            fn wants_write(&self) -> bool {
                (**self).wants_write()
            }
            fn is_handshaking(&self) -> bool {
                (**self).is_handshaking()
            }
            fn send_close_notify(&mut self) {
                (**self).send_close_notify()
            }
            fn alpn_protocol(&self) -> Option<&[u8]> {
                (**self).alpn_protocol()
            }
            fn peer_certificates(&self) -> Option<&[CertificateDer<'static>]> {
                (**self).peer_certificates()
            }
            fn handshake_kind(&self) -> Option<rustls::HandshakeKind> {
                (**self).handshake_kind()
            }
        }
    };
}

side!(ClientConnection, ClientConfig);
side!(ServerConnection, ServerConfig);

/// The transport as `std::io` for one poll: `Pending` becomes `WouldBlock` (the transport has
/// registered the waker). hyper-tls's `read_tls` / `write_tls` take `std::io`.
struct SyncIo<'a, 'b, S> {
    io: &'a mut S,
    cx: &'a mut Context<'b>,
}

impl<S: AsyncRead + Unpin> io::Read for SyncIo<'_, '_, S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match Pin::new(&mut *self.io).poll_read(self.cx, buf) {
            Poll::Ready(result) => result,
            Poll::Pending => Err(io::ErrorKind::WouldBlock.into()),
        }
    }
}

impl<S: AsyncWrite + Unpin> io::Write for SyncIo<'_, '_, S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match Pin::new(&mut *self.io).poll_write(self.cx, buf) {
            Poll::Ready(result) => result,
            Poll::Pending => Err(io::ErrorKind::WouldBlock.into()),
        }
    }

    fn write_vectored(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
        match Pin::new(&mut *self.io).poll_write_vectored(self.cx, bufs) {
            Poll::Ready(result) => result,
            Poll::Pending => Err(io::ErrorKind::WouldBlock.into()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match Pin::new(&mut *self.io).poll_flush(self.cx) {
            Poll::Ready(result) => result,
            Poll::Pending => Err(io::ErrorKind::WouldBlock.into()),
        }
    }
}

fn tls_error(error: rustls::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

/// A TLS session over the transport `S`, owning its configuration: a client's
/// ([`TlsStream`]) or a server's ([`ServerTlsStream`]). Plaintext through futures-io, which
/// kiln-http (and anything else, such as a byte pipe) speaks.
pub struct Stream<S, C: Side> {
    io: S,
    conn: C,
    config: C::Config,
    eof: bool,
    closing: bool,
}

/// A client TLS session.
pub type TlsStream<S> = Stream<S, ClientConnection>;
/// A server TLS session.
pub type ServerTlsStream<S> = Stream<S, ServerConnection>;

impl<S: AsyncRead + AsyncWrite + Unpin> Stream<S, ClientConnection> {
    /// Start a connection to `name` over `io` and complete the handshake.
    pub async fn connect(io: S, mut config: ClientConfig, name: ServerName<'static>) -> io::Result<Self> {
        let conn = ClientConnection::new(&mut config, name).map_err(tls_error)?;
        let mut stream = Self { io, conn, config, eof: false, closing: false };
        poll_fn(|cx| stream.poll_handshake(cx)).await?;
        Ok(stream)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> Stream<S, ServerConnection> {
    /// Answer a client over `io` and complete the handshake.
    pub async fn accept(io: S, config: ServerConfig) -> io::Result<Self> {
        let conn = ServerConnection::new(&config).map_err(tls_error)?;
        let mut stream = Self { io, conn, config, eof: false, closing: false };
        poll_fn(|cx| stream.poll_handshake(cx)).await?;
        Ok(stream)
    }

    /// The name the client asked for (SNI), if it sent one.
    pub fn server_name(&self) -> Option<&str> {
        self.conn.server_name()
    }
}

impl<S, C: Side> Stream<S, C> {
    /// The protocol agreed in the handshake (ALPN), if any: [`H2`], [`HTTP11`].
    pub fn alpn_protocol(&self) -> Option<&[u8]> {
        self.conn.alpn_protocol()
    }

    /// The certificates the peer presented, its own first (a server's chain to a client).
    pub fn peer_certificates(&self) -> Option<&[CertificateDer<'static>]> {
        self.conn.peer_certificates()
    }

    /// What the handshake was, as `kiln_tls_handshakes_total` counts it: `full`,
    /// `full_hello_retry` (a full one after a HelloRetryRequest) or `resumed`.
    pub fn handshake(&self) -> &'static str {
        match self.conn.handshake_kind() {
            Some(rustls::HandshakeKind::Resumed) => "resumed",
            Some(rustls::HandshakeKind::FullWithHelloRetryRequest) => "full_hello_retry",
            _ => "full",
        }
    }

    /// The configuration back, for the next connection (a client's, holding its tickets).
    #[cfg(test)]
    pub(crate) fn into_config(self) -> C::Config {
        self.config
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin, C: Side> Stream<S, C> {
    fn poll_write_tls(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let mut io = SyncIo { io: &mut self.io, cx };
        match self.conn.write_tls(&mut io) {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Poll::Pending,
            result => Poll::Ready(result),
        }
    }

    fn poll_flush_tls(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while self.conn.wants_write() {
            if ready!(self.poll_write_tls(cx))? == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
        }
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_read_tls(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let mut io = SyncIo { io: &mut self.io, cx };
        let read = match self.conn.read_tls(&mut io) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Poll::Pending,
            Err(error) => return Poll::Ready(Err(error)),
        };
        if read == 0 {
            self.eof = true;
            return Poll::Ready(Ok(0));
        }
        match self.conn.process_new_packets(&mut self.config) {
            Ok(state) if state.peer_has_closed() && self.conn.is_handshaking() => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "close_notify during the TLS handshake",
            ))),
            Ok(_) => Poll::Ready(Ok(read)),
            Err(error) => {
                // The alert telling the peer why is queued: a last-gasp write that must not mask it.
                let _ = self.poll_write_tls(cx);
                Poll::Ready(Err(tls_error(error)))
            }
        }
    }

    fn poll_handshake(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        loop {
            ready!(self.poll_flush_tls(cx))?;
            if !self.conn.is_handshaking() {
                return Poll::Ready(Ok(()));
            }
            if self.eof {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "transport closed during the TLS handshake",
                )));
            }
            ready!(self.poll_read_tls(cx))?;
        }
    }

    /// Ready once the reader has an answer: plaintext, a clean end, or an unclean one.
    fn poll_plaintext(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.conn.is_handshaking() {
            ready!(self.poll_handshake(cx))?;
        }
        loop {
            match self.conn.reader().fill_buf() {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                _ => return Poll::Ready(Ok(())),
            }
            while self.conn.wants_write() {
                match self.poll_write_tls(cx) {
                    Poll::Ready(Ok(0)) => return Poll::Ready(Err(io::ErrorKind::WriteZero.into())),
                    Poll::Ready(Ok(_)) => {}
                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                    Poll::Pending => break,
                }
            }
            ready!(self.poll_read_tls(cx))?;
        }
    }

    /// Plaintext into `buf`; 0 at a clean end (close_notify, or the transport's end after one).
    fn poll_read_plain(&mut self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        ready!(self.poll_plaintext(cx))?;
        match self.conn.reader().read(buf) {
            // The transport ended without close_notify: an end all the same for a byte pipe
            // (truncation attacks matter to framing that the application, HTTP, does itself).
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Poll::Ready(Ok(0)),
            other => Poll::Ready(other),
        }
    }

    fn poll_write_plain(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        if self.conn.is_handshaking() {
            ready!(self.poll_handshake(cx))?;
        }
        let mut taken = 0usize;
        loop {
            let mut blocked = false;
            while self.conn.wants_write() {
                match self.poll_write_tls(cx) {
                    Poll::Ready(Ok(0)) => return Poll::Ready(Err(io::ErrorKind::WriteZero.into())),
                    Poll::Ready(Ok(_)) => {}
                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                    Poll::Pending => {
                        blocked = true;
                        break;
                    }
                }
            }
            let rest = buf.get(taken..).unwrap_or_default();
            if rest.is_empty() {
                return Poll::Ready(Ok(taken));
            }
            let took = self.conn.writer().write(rest)?;
            if took == 0 {
                return if taken > 0 {
                    Poll::Ready(Ok(taken))
                } else if blocked {
                    Poll::Pending
                } else {
                    Poll::Ready(Err(io::ErrorKind::WriteZero.into()))
                };
            }
            taken = taken.saturating_add(took);
        }
    }

    fn poll_flush_plain(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.conn.is_handshaking() {
            ready!(self.poll_handshake(cx))?;
        }
        self.conn.writer().flush()?;
        self.poll_flush_tls(cx)
    }

    fn poll_close_plain(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if !self.closing {
            self.conn.send_close_notify();
            self.closing = true;
        }
        ready!(self.poll_flush_tls(cx))?;
        Pin::new(&mut self.io).poll_close(cx)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin, C: Side + Unpin> AsyncRead for Stream<S, C>
where
    C::Config: Unpin,
{
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        self.get_mut().poll_read_plain(cx, buf)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin, C: Side + Unpin> AsyncWrite for Stream<S, C>
where
    C::Config: Unpin,
{
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        self.get_mut().poll_write_plain(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().poll_flush_plain(cx)
    }

    /// Sends close_notify, then ends the transport's write side.
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().poll_close_plain(cx)
    }
}

/// Plaintext a read at a time, for code that wants to read without futures-io.
impl<S: AsyncRead + AsyncWrite + Unpin, C: Side> Stream<S, C> {
    pub async fn read_some(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        poll_fn(|cx| {
            ready!(self.poll_plaintext(cx))?;
            Poll::Ready(self.conn.reader().read(buf))
        })
        .await
    }
}

/// A transport whose first bytes were already read (to tell TLS from plain HTTP): they are read
/// again first, then the rest comes from the transport.
pub struct Prefixed<S> {
    head: Vec<u8>,
    at: usize,
    io: S,
}

impl<S> Prefixed<S> {
    pub fn new(head: Vec<u8>, io: S) -> Self {
        Self { head, at: 0, io }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Prefixed<S> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if let Some(rest) = this.head.get(this.at..)
            && !rest.is_empty()
        {
            let n = rest.len().min(buf.len());
            if let (Some(dst), Some(src)) = (buf.get_mut(..n), rest.get(..n)) {
                dst.copy_from_slice(src);
            }
            this.at += n;
            return Poll::Ready(Ok(n));
        }
        Pin::new(&mut this.io).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Prefixed<S> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().io).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_close(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() -> anyhow::Result<()> {
        assert!(matches!(server_name("api.github.com")?, ServerName::DnsName(_)));
        assert!(matches!(server_name("192.168.49.2")?, ServerName::IpAddress(_)));
        assert!(matches!(server_name("[::1]")?, ServerName::IpAddress(_)));
        Ok(())
    }

    #[test]
    fn configs_build() -> anyhow::Result<()> {
        let tls = ClientTls::new(Roots::WebPki, None)?;
        let cfg = tls.config()?;
        assert_eq!(cfg.alpn_protocols, vec![b"http/1.1".to_vec()]);
        Ok(())
    }

    #[test]
    fn server_certificates_must_match_their_keys() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pki = crate::pki::Pki::open(dir.path())?;
        let leaf = pki.server_leaf(&[])?;
        let tls = ServerTls::from_pem(&leaf.cert_pem, &leaf.key_pem)?;
        assert!(tls.leaf().is_some_and(|d| !d.is_empty()));
        assert_eq!(tls.config()?.alpn_protocols, vec![b"http/1.1".to_vec()]);
        // Another key, no certificate, no key: refused, and the errors quote no key material.
        let other = crate::pki::new_key_pem()?;
        let wrong = ServerTls::from_pem(&leaf.cert_pem, &other).err().map(|e| format!("{e:#}")).unwrap_or_default();
        assert!(wrong.contains("not the certificate's"), "{wrong}");
        assert!(!wrong.contains("PRIVATE KEY-----"));
        assert!(ServerTls::from_pem("", &leaf.key_pem).is_err());
        assert!(ServerTls::from_pem(&leaf.cert_pem, "nonsense").is_err());
        assert!(!format!("{tls:?}").contains("BEGIN"));
        Ok(())
    }

    /// A client and a server of these streams agree over a real TCP connection, both ways, and
    /// the client checks the server's certificate against kiln's CA under the name it dialled.
    #[test]
    fn a_tls_server_and_client_talk_over_tcp() -> anyhow::Result<()> {
        use futures::{AsyncReadExt as _, AsyncWriteExt as _};
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let pki = crate::pki::Pki::open(dir.path())?;
            let key = crate::pki::new_key_pem()?;
            let csr = crate::pki::front_door_csr(&["kiln.test".to_owned()], &key)?;
            let cert = pki.sign_front_door(&csr, &["kiln.test".to_owned(), "*.kiln.test".to_owned()], 30)?;
            let server = ServerTls::from_pem(&cert, &key)?;
            let listener = hyper_rt::tcp::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), 16).map_err(|e| anyhow::anyhow!("{e}"))?;
            let addr = listener.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
            let served = crate::rt::spawn(async move {
                let tcp = listener.accept().await.map_err(|e| anyhow::anyhow!("{e}"))?;
                let mut tls = ServerTlsStream::accept(super::super::io::RtFuturesIo::new(tcp), server.config()?).await?;
                let sni = tls.server_name().map(str::to_owned);
                let mut got = [0u8; 5];
                tls.read_exact(&mut got).await?;
                tls.write_all(b"world").await?;
                tls.flush().await?;
                tls.close().await?;
                Ok::<_, anyhow::Error>((sni, got))
            });
            let roots = ClientTls::new(Roots::Pem(pki.ca_pem.clone().into_bytes()), None)?;
            let tcp = hyper_rt::tcp::TcpStream::connect(addr).await.map_err(|e| anyhow::anyhow!("{e}"))?;
            let mut client = TlsStream::connect(super::super::io::RtFuturesIo::new(tcp), roots.config()?, server_name("www.kiln.test")?).await?;
            client.write_all(b"hello").await?;
            client.flush().await?;
            let mut back = Vec::new();
            client.read_to_end(&mut back).await?;
            assert_eq!(back, b"world");
            let (sni, got) = served.await.map_err(|_| anyhow::anyhow!("server task"))??;
            assert_eq!(&got, b"hello");
            assert_eq!(sni.as_deref(), Some("www.kiln.test"));

            // A name the certificate does not carry is refused by the client.
            let listener = hyper_rt::tcp::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), 16).map_err(|e| anyhow::anyhow!("{e}"))?;
            let addr = listener.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
            let server = ServerTls::from_pem(&cert, &key)?;
            let _served = crate::rt::spawn(async move {
                if let Ok(tcp) = listener.accept().await
                    && let Ok(cfg) = server.config()
                {
                    let _ = ServerTlsStream::accept(super::super::io::RtFuturesIo::new(tcp), cfg).await;
                }
            });
            let tcp = hyper_rt::tcp::TcpStream::connect(addr).await.map_err(|e| anyhow::anyhow!("{e}"))?;
            assert!(TlsStream::connect(super::super::io::RtFuturesIo::new(tcp), roots.config()?, server_name("kiln.home")?).await.is_err());
            Ok(())
        })
    }

    /// A front-door certificate from kiln's CA for `names` (the kiln leaf's shape), and its key.
    fn kiln_leaf(pki: &crate::pki::Pki, names: &[&str]) -> anyhow::Result<(String, String)> {
        let names: Vec<String> = names.iter().map(|n| (*n).to_owned()).collect();
        let key = crate::pki::new_key_pem()?;
        let csr = crate::pki::front_door_csr(&names, &key)?;
        Ok((pki.sign_front_door(&csr, &names, 30)?, key))
    }

    /// Records from one end to the other until neither has more to say: the bytes each way, and
    /// the client's first flight (its ClientHello), kept to be replayed.
    struct Pumped {
        up: usize,
        down: usize,
        hello: Vec<u8>,
    }

    fn pump(client: &mut ClientConnection, ccfg: &mut ClientConfig, server: &mut ServerConnection, scfg: &mut ServerConfig) -> anyhow::Result<Pumped> {
        let mut p = Pumped { up: 0, down: 0, hello: Vec::new() };
        for _ in 0..16 {
            let mut up = Vec::new();
            while client.wants_write() {
                client.write_tls(&mut up)?;
            }
            if p.hello.is_empty() {
                p.hello.clone_from(&up);
            }
            p.up += up.len();
            let mut rd = up.as_slice();
            while !rd.is_empty() {
                server.read_tls(&mut rd)?;
                server.process_new_packets(scfg)?;
            }
            let mut down = Vec::new();
            while server.wants_write() {
                server.write_tls(&mut down)?;
            }
            p.down += down.len();
            let mut rd = down.as_slice();
            while !rd.is_empty() {
                client.read_tls(&mut rd)?;
                client.process_new_packets(ccfg)?;
            }
            if up.is_empty() && down.is_empty() && !client.is_handshaking() && !server.is_handshaking() {
                return Ok(p);
            }
        }
        anyhow::bail!("the handshake didn't settle")
    }

    /// One connection, in memory, from the client's configuration to the listener's.
    fn connect(ccfg: &mut ClientConfig, server: &ServerTls) -> anyhow::Result<(Pumped, Option<rustls::HandshakeKind>, Option<rustls::HandshakeKind>)> {
        let mut client = ClientConnection::new(ccfg, server_name("kiln.test")?)?;
        let mut scfg = server.config()?;
        let mut conn = ServerConnection::new(&scfg)?;
        let p = pump(&mut client, ccfg, &mut conn, &mut scfg)?;
        Ok((p, client.handshake_kind(), conn.handshake_kind()))
    }

    /// Goal 7 step 3 (plan 01a1a832ebbf7327a203df446e16ecac), "the ledger client resumes with a
    /// ticket from a previous connection to the same listener": the second connection from one
    /// client configuration resumes (the PSK accepted, so no Certificate or CertificateVerify in
    /// the server's flight, RFC 8446 §2.2), and the server sends at least 25% fewer handshake
    /// bytes than for the full one: same client, same key exchange (X25519MLKEM768), kiln's leaf.
    #[test]
    fn g7_s3_a_ticket_from_one_connection_resumes_the_next() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pki = crate::pki::Pki::open(dir.path())?;
        let (cert, key) = kiln_leaf(&pki, &["kiln.test"])?;
        let server = ServerTls::from_pem(&cert, &key)?;
        let mut ccfg = ClientTls::new(Roots::Pem(pki.ca_pem.clone().into_bytes()), None)?.config()?;
        let (full, ck, sk) = connect(&mut ccfg, &server)?;
        assert_eq!((ck, sk), (Some(rustls::HandshakeKind::Full), Some(rustls::HandshakeKind::Full)));
        assert_eq!(server.sessions().len(), 2, "the listener keeps both tickets it sent");
        let (resumed, ck, sk) = connect(&mut ccfg, &server)?;
        assert_eq!((ck, sk), (Some(rustls::HandshakeKind::Resumed), Some(rustls::HandshakeKind::Resumed)));
        // One ticket used, two new ones: three kept.
        assert_eq!(server.sessions().len(), 3);
        let fall = 1.0 - resumed.down as f64 / full.down as f64;
        assert!(fall >= 0.25, "server-to-client handshake bytes: full {} B, resumed {} B ({:.1}% less)", full.down, resumed.down, fall * 100.0);
        let leaf = server.leaf().map_or(0, <[u8]>::len);
        assert!(full.down.saturating_sub(resumed.down) >= leaf, "the certificate ({leaf} B) isn't sent again: full {} B, resumed {} B", full.down, resumed.down);
        // The third resumes too, with the other ticket of the first.
        let (_, ck, _) = connect(&mut ccfg, &server)?;
        assert_eq!(ck, Some(rustls::HandshakeKind::Resumed));
        Ok(())
    }

    /// Goal 7 step 3, "the same ticket offered twice": the ClientHello of a resumed connection,
    /// sent again whole (its ticket, its binder), gets a full handshake: the session went from the
    /// store the first time it was used.
    #[test]
    fn g7_s3_a_ticket_resumes_once() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pki = crate::pki::Pki::open(dir.path())?;
        let (cert, key) = kiln_leaf(&pki, &["kiln.test"])?;
        let server = ServerTls::from_pem(&cert, &key)?;
        let mut ccfg = ClientTls::new(Roots::Pem(pki.ca_pem.clone().into_bytes()), None)?.config()?;
        connect(&mut ccfg, &server)?;
        let (resumed, _, sk) = connect(&mut ccfg, &server)?;
        assert_eq!(sk, Some(rustls::HandshakeKind::Resumed));
        let mut scfg = server.config()?;
        let mut again = ServerConnection::new(&scfg)?;
        let mut rd = resumed.hello.as_slice();
        while !rd.is_empty() {
            again.read_tls(&mut rd)?;
            again.process_new_packets(&mut scfg)?;
        }
        assert_eq!(again.handshake_kind(), Some(rustls::HandshakeKind::Full), "a replayed ticket is refused");
        Ok(())
    }

    /// Goal 7 step 3, "store at capacity": the oldest session goes first, the store never holds
    /// more than its capacity (nor a session over [`SESSION_BYTES`]), and nothing panics; a session
    /// taken is gone, and one past its lifetime isn't resumed.
    #[test]
    fn g7_s3_the_store_at_capacity_lets_its_oldest_go() {
        let sessions = Sessions::new(Resumption { capacity: 3, ..Resumption::default() });
        let key = |i: u32| i.to_be_bytes().repeat(8);
        for i in 0..5 {
            assert!(sessions.put(key(i), vec![7; 100]));
        }
        assert_eq!(sessions.len(), 3);
        assert_eq!(sessions.bytes(), 3 * (32 + 100));
        assert!(sessions.get(&key(0)).is_none() && sessions.get(&key(1)).is_none(), "the oldest went first");
        assert!(sessions.get(&key(2)).is_some(), "get leaves it (TLS 1.2's session ids)");
        assert!(sessions.take(&key(4)).is_some());
        assert!(sessions.take(&key(4)).is_none(), "used once, refused after");
        assert_eq!(sessions.len(), 2);
        // Many more, and the same key again: still at the bound, in bytes too.
        for i in 0..10_000 {
            sessions.put(key(i % 7), vec![1; 200]);
        }
        assert_eq!(sessions.len(), 3);
        assert_eq!(sessions.bytes(), 3 * (32 + 200));
        assert_eq!(sessions.inner.lock().order.len(), 3, "the eviction order holds only what's kept");
        assert!(!sessions.put(key(1), vec![0; SESSION_BYTES]), "too big to keep");
        // Lifetimes: none past it, and never beyond what hyper-tls tells clients.
        let expired = Sessions::new(Resumption { lifetime: Duration::ZERO, ..Resumption::default() });
        assert!(expired.put(key(1), vec![1; 10]));
        assert!(expired.take(&key(1)).is_none());
        assert_eq!(Sessions::new(Resumption { lifetime: Duration::from_secs(7 * 86_400), ..Resumption::default() }).resumption().lifetime, STATEFUL_LIFETIME);
        // No room at all: nothing kept, and hyper-tls is told so.
        let none = Sessions::new(Resumption { capacity: 0, ..Resumption::default() });
        assert!(!none.put(key(1), vec![1; 10]));
        assert!(!SharedSessions(Arc::new(none)).can_cache());
    }

    /// Goal 7 step 3: a renewed certificate (the same names) keeps the listener's sessions; one for
    /// other names starts with none. Either way new connections get the new certificate.
    #[test]
    fn g7_s3_a_renewal_keeps_the_sessions_other_names_drop_them() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pki = crate::pki::Pki::open(dir.path())?;
        let (cert, key) = kiln_leaf(&pki, &["kiln.test", "www.kiln.test"])?;
        let old = ServerTls::from_pem(&cert, &key)?;
        let mut ccfg = ClientTls::new(Roots::Pem(pki.ca_pem.clone().into_bytes()), None)?.config()?;
        connect(&mut ccfg, &old)?;
        let (cert, key) = kiln_leaf(&pki, &["www.kiln.test", "kiln.test"])?;
        let renewed = ServerTls::from_pem(&cert, &key)?.succeeding(&old);
        assert!(Arc::ptr_eq(renewed.sessions(), old.sessions()));
        assert_ne!(renewed.leaf(), old.leaf());
        let (_, ck, _) = connect(&mut ccfg, &renewed)?;
        assert_eq!(ck, Some(rustls::HandshakeKind::Resumed), "resumed across the renewal");
        let (cert, key) = kiln_leaf(&pki, &["kiln.test", "other.test"])?;
        let other = ServerTls::from_pem(&cert, &key)?.succeeding(&renewed);
        assert!(other.sessions().is_empty());
        let mut client = ClientConnection::new(&mut ccfg, server_name("kiln.test")?)?;
        let mut scfg = other.config()?;
        let mut conn = ServerConnection::new(&scfg)?;
        pump(&mut client, &mut ccfg, &mut conn, &mut scfg)?;
        assert_eq!(client.handshake_kind(), Some(rustls::HandshakeKind::Full));
        assert_eq!(client.peer_certificates().and_then(|c| c.first()).map(|c| c.as_ref()), other.leaf(), "the new certificate");
        Ok(())
    }
}
