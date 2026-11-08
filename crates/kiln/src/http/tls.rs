//! TLS on hyper-tls (hyper-raft's rustls fork, reached as `hyper_quic::rustls`), driven over any
//! futures-io transport, for both ends: HTTPS clients ([`ClientTls`], [`TlsStream`]) and servers
//! ([`ServerTls`], [`ServerTlsStream`]: the server's own listener with `server.https`, and a
//! runner's front door). No tokio, no tokio-rustls.
//!
//! hyper-tls connections hold no configuration: every call that may advance the state machine
//! (`process_new_packets(&mut config)`) borrows it, for the connection's whole life (a TLS 1.3
//! client files NewSessionTickets into it after the handshake). So each connection owns its own
//! configuration, built from shared material: a root store ([`ClientTls::config`]), or a
//! certificate and its key, parsed once, and the listener's ticket keys ([`ServerTls::config`]).
//!
//! A server's sessions outlive the connection that made them (goal 7 step 3, plan
//! 01a1a832ebbf7327a203df446e16ecac): each is sealed into the tickets the server sends, under a
//! key its [`ServerTls`] holds for every connection ([`TicketKeys`]), so a browser coming back
//! resumes, without the certificate and its signature, rather than starting again. No session is
//! kept on the server and no lock is taken: a ticket resumes on any shard.

use std::future::poll_fn;
use std::io::{self, BufRead as _, Read as _, Write as _};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use anyhow::Context as _;
use aws_lc_rs::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use futures::io::{AsyncRead, AsyncWrite};
use hyper_quic::rustls::crypto::CryptoProvider;
use hyper_quic::rustls::pki_types::pem::PemObject as _;
use hyper_quic::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use hyper_quic::rustls::server::{ClientHello, NoServerSessionStorage, ProducesTickets, ResolvesServerCert};
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

/// How a TLS listener lets browsers resume (goal 7 step 3): stateless tickets, the session
/// sealed into the ticket under the listener's ticket key ([`TicketKeys`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resumption {
    /// How long a ticket resumes after it was issued, as clients are told (at most
    /// [`MAX_TICKET_LIFETIME`]); also how long a key seals new tickets before the next takes over.
    pub lifetime: Duration,
    /// TLS 1.3 tickets sent after each handshake.
    pub tickets: usize,
}

/// How long a ticket resumes: 24 hours, the lifetime hyper-tls gave its stateful tickets before.
pub const TICKET_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);

/// The longest a TLS 1.3 ticket may live (RFC 8446 §4.6.1: 7 days).
pub const MAX_TICKET_LIFETIME: Duration = Duration::from_secs(7 * 24 * 60 * 60);

impl Default for Resumption {
    /// Tickets resumable for 24 hours, and hyper-tls's 2 tickets a handshake (as before). Ada's to
    /// confirm against the lab's count of connections per page load (goal 7 step 2).
    fn default() -> Self {
        Self { lifetime: TICKET_LIFETIME, tickets: 2 }
    }
}

/// A ticket key's name, at the head of each ticket it seals: which key opens it.
const KEY_NAME: usize = 8;

/// One ticket key: an AES-256-GCM key and its random name.
struct TicketKey {
    name: [u8; KEY_NAME],
    aead: LessSafeKey,
}

impl TicketKey {
    fn new() -> Option<Self> {
        let mut name = [0u8; KEY_NAME];
        let mut key = [0u8; 32];
        aws_lc_rs::rand::fill(&mut name).ok()?;
        aws_lc_rs::rand::fill(&mut key).ok()?;
        let aead = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &key).ok()?);
        Some(Self { name, aead })
    }
}

/// The keys in use at once, an immutable copy: the one sealing tickets now, and the one before it
/// while tickets it sealed may still be resumed.
#[derive(Clone)]
struct KeyRing {
    current: Arc<TicketKey>,
    previous: Option<Arc<TicketKey>>,
    /// When `current` stops sealing tickets (Unix seconds): the next key takes over then.
    until: u64,
}

/// One listener's ticket keys (goal 7 step 3, lockless for the API runtime): a [`KeyRing`] read
/// without a lock and swapped in whole when a key is due to go (`crate::rcu::Rcu`), so sealing and
/// opening a ticket waits on no one, and a ticket from one connection resumes on any shard.
///
/// A ticket is the key's name, the second it was issued, a nonce and the session sealed with
/// AES-256-GCM (the name and the second authenticated with it): `name(8) issued(8) nonce(12)
/// sealed`. It resumes while it is younger than [`Resumption::lifetime`] and its key is kept. A key
/// seals tickets for one lifetime, then the next takes over (made on the first handshake past
/// it), and it is kept one more lifetime, until the last ticket it sealed is too old: so at most
/// two keys, the newest at most a lifetime old when it seals. In memory only: a restart makes a
/// new key, and every browser's next connect is a full handshake.
///
/// Stateless: a ticket is not used up, so the same ticket may resume more than once. Harmless with
/// 0-RTT off (kiln accepts no early data): a replayed ClientHello carries no request, and the
/// handshake it starts still needs the client's own key exchange (`psk_dhe_ke`).
pub struct TicketKeys {
    resumption: Resumption,
    ring: crate::rcu::Rcu<KeyRing>,
}

impl std::fmt::Debug for TicketKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the keys.
        f.debug_struct("TicketKeys").field("resumption", &self.resumption).field("until", &self.ring.load().until).finish()
    }
}

/// Seconds since the Unix epoch, now.
fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl TicketKeys {
    /// Keys for `resumption` (its lifetime at most [`MAX_TICKET_LIFETIME`]), the first made now.
    pub fn new(resumption: Resumption) -> anyhow::Result<Self> {
        Self::new_at(resumption, unix_now())
    }

    fn new_at(resumption: Resumption, now: u64) -> anyhow::Result<Self> {
        let resumption = Resumption { lifetime: resumption.lifetime.min(MAX_TICKET_LIFETIME), ..resumption };
        let current = Arc::new(TicketKey::new().context("a TLS ticket key")?);
        let until = now.saturating_add(resumption.lifetime.as_secs());
        Ok(Self { resumption, ring: crate::rcu::Rcu::new(KeyRing { current, previous: None, until }) })
    }

    pub fn resumption(&self) -> Resumption {
        self.resumption
    }

    fn lifetime_s(&self) -> u64 {
        self.resumption.lifetime.as_secs()
    }

    /// `f` on the keys at `now`: past the current key's time, a new key first, swapped in whole (by
    /// whichever handshake gets there first; the others see it). Should no key be made, the current
    /// one stays and seals nothing more ([`Self::seal_at`]).
    fn with_ring<R>(&self, now: u64, f: impl FnOnce(&KeyRing) -> R) -> R {
        let ring = self.ring.load();
        if now < ring.until {
            return f(&ring);
        }
        drop(ring);
        if let Some(fresh) = TicketKey::new().map(Arc::new) {
            self.roll(&fresh, now, false);
        }
        f(&self.ring.load())
    }

    /// `fresh` the current key from `now`, if the current one is due to go (or `force`): the one it
    /// replaces is kept while the tickets it sealed (until its time, at the latest) may resume.
    fn roll(&self, fresh: &Arc<TicketKey>, now: u64, force: bool) {
        let life = self.lifetime_s();
        self.ring.update(|r| {
            if force || now >= r.until {
                let sealed_until = r.until.min(now);
                let old = std::mem::replace(&mut r.current, Arc::clone(fresh));
                r.previous = (now < sealed_until.saturating_add(life)).then_some(old);
                r.until = now.saturating_add(life);
            }
        });
    }

    /// A new key from now on, the current one kept as the previous (a test's rotation, without
    /// waiting a lifetime).
    #[cfg(test)]
    fn rotate(&self) -> anyhow::Result<()> {
        let fresh = Arc::new(TicketKey::new().context("a TLS ticket key")?);
        self.roll(&fresh, unix_now(), true);
        Ok(())
    }

    /// `plain` (a session) sealed into a ticket at `now` by the current key.
    fn seal_at(&self, now: u64, plain: &[u8]) -> Option<Vec<u8>> {
        self.with_ring(now, |ring| Self::seal(ring, now, plain))
    }

    fn seal(ring: &KeyRing, now: u64, plain: &[u8]) -> Option<Vec<u8>> {
        if now >= ring.until {
            return None;
        }
        let mut nonce = [0u8; NONCE_LEN];
        aws_lc_rs::rand::fill(&mut nonce).ok()?;
        let mut ticket = Vec::with_capacity(plain.len().saturating_add(KEY_NAME + 8 + NONCE_LEN + AES_256_GCM.tag_len()));
        ticket.extend_from_slice(&ring.current.name);
        ticket.extend_from_slice(&now.to_be_bytes());
        let mut sealed = plain.to_vec();
        ring.current.aead.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(ticket.as_slice()), &mut sealed).ok()?;
        ticket.extend_from_slice(&nonce);
        ticket.extend_from_slice(&sealed);
        Some(ticket)
    }

    /// The session in `ticket` (untrusted bytes) at `now`: none for a ticket that's too old, or
    /// from the future, or whose key is gone, or that doesn't open (any byte changed).
    fn open_at(&self, now: u64, ticket: &[u8]) -> Option<Vec<u8>> {
        self.with_ring(now, |ring| self.open(ring, now, ticket))
    }

    fn open(&self, ring: &KeyRing, now: u64, ticket: &[u8]) -> Option<Vec<u8>> {
        let (name, rest) = ticket.split_first_chunk::<KEY_NAME>()?;
        let (issued, rest) = rest.split_first_chunk::<8>()?;
        let (nonce, sealed) = rest.split_first_chunk::<NONCE_LEN>()?;
        let age = now.checked_sub(u64::from_be_bytes(*issued))?;
        if age >= self.lifetime_s() {
            return None;
        }
        let aad = ticket.get(..KEY_NAME + 8)?;
        let keys = std::iter::once(&ring.current).chain(ring.previous.as_ref());
        for key in keys.filter(|k| k.name == *name) {
            let mut out = sealed.to_vec();
            if let Ok(plain) = key.aead.open_in_place(Nonce::assume_unique_for_key(*nonce), Aad::from(aad), &mut out) {
                let n = plain.len();
                out.truncate(n);
                return Some(out);
            }
        }
        None
    }
}

/// A connection configuration's handle on its listener's [`TicketKeys`] (hyper-tls's
/// `ProducesTickets`, over shared keys read without a lock; hyper-tls itself is unchanged).
#[derive(Debug)]
struct SharedTickets(Arc<TicketKeys>);

impl ProducesTickets for SharedTickets {
    fn enabled(&self) -> bool {
        true
    }

    fn lifetime(&self) -> u32 {
        u32::try_from(self.0.lifetime_s()).unwrap_or(u32::MAX)
    }

    fn encrypt(&mut self, plain: &[u8]) -> Option<Vec<u8>> {
        self.0.seal_at(unix_now(), plain)
    }

    fn decrypt(&mut self, cipher: &[u8]) -> Option<Vec<u8>> {
        self.0.open_at(unix_now(), cipher)
    }
}

/// A server's certificate chain and key, parsed (and checked to belong together) once, and the
/// ticket keys of the listener it serves ([`TicketKeys`]); each connection's configuration is made
/// from them ([`Self::config`]). Cheap to clone: clones share the certificate and the keys.
#[derive(Clone)]
pub struct ServerTls {
    key: Arc<CertifiedKey>,
    /// The leaf's names (its subject alternative names), sorted: which tickets it may resume.
    names: Arc<[String]>,
    tickets: Arc<TicketKeys>,
}

impl std::fmt::Debug for ServerTls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerTls").field("chain", &self.key.cert.len()).field("key", &"<redacted>").field("tickets", &self.tickets).finish()
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
    /// leaf's. Errors never quote the key. Its ticket keys are new ([`Resumption::default`]).
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
        let tls = Self { key: Arc::new(key), names: names.into(), tickets: Arc::new(TicketKeys::new(Resumption::default())?) };
        tls.config()?;
        Ok(tls)
    }

    /// The leaf certificate (DER).
    pub fn leaf(&self) -> Option<&[u8]> {
        self.key.cert.first().map(|c| c.as_ref())
    }

    /// The listener's ticket keys.
    pub fn tickets(&self) -> &Arc<TicketKeys> {
        &self.tickets
    }

    /// This certificate in place of `old` on the same listener: a renewal (a leaf for the same
    /// names) keeps `old`'s ticket keys, so browsers resume across it; a certificate for other
    /// names gets keys of its own, so no earlier ticket resumes, and every browser's next connect
    /// is a full handshake that sees it.
    pub fn succeeding(self, old: &ServerTls) -> Self {
        if !self.names.is_empty() && self.names == old.names {
            Self { tickets: Arc::clone(&old.tickets), ..self }
        } else {
            self
        }
    }

    /// A fresh configuration for one connection (TLS 1.3 and 1.2, ALPN http/1.1, no client
    /// certificates), its tickets sealed with the listener's keys.
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
        // Stateless tickets, nothing kept here (TLS 1.2 resumes by ticket too, not by session id),
        // and no early data: a ticket may be replayed, so 0-RTT stays off.
        config.session_storage = Box::new(NoServerSessionStorage {});
        config.ticketer = Box::new(SharedTickets(Arc::clone(&self.tickets)));
        config.send_tls13_tickets = self.tickets.resumption.tickets;
        config.max_early_data_size = 0;
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
    /// Bounds what the session buffers to send (`None`: unbounded).
    fn set_buffer_limit(&mut self, limit: Option<usize>);
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
            fn set_buffer_limit(&mut self, limit: Option<usize>) {
                (**self).set_buffer_limit(limit)
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
    /// The bound on what TLS buffers to send, when someone sets it as the session goes (the browser
    /// protocol's socket sets its chunk: `crate::live`), and the bound last applied (0: TLS's own).
    send_limit: Option<std::sync::Arc<SendBound>>,
    applied: usize,
    /// The plaintext TLS took that it hasn't written to the transport yet, as far as it can tell (all it
    /// took since it last had nothing to write), when someone reads it ([`SendBound`]).
    held: u64,
}

/// What the browser protocol's socket sets and reads of TLS's send buffer (`crate::live`, plan
/// `01a1a832eb2f711f9c6f51d88effe89d` step 3): the bound it sets (0: TLS's own, 64 KiB), and the
/// plaintext TLS holds unwritten to the socket, now and at most. Written by the session, read by
/// anyone, never locked.
#[derive(Debug, Default)]
pub struct SendBound {
    pub limit: std::sync::atomic::AtomicUsize,
    pub held: std::sync::atomic::AtomicU64,
    pub held_max: std::sync::atomic::AtomicU64,
}

/// A client TLS session.
pub type TlsStream<S> = Stream<S, ClientConnection>;
/// A server TLS session.
pub type ServerTlsStream<S> = Stream<S, ServerConnection>;

impl<S: AsyncRead + AsyncWrite + Unpin> Stream<S, ClientConnection> {
    /// Start a connection to `name` over `io` and complete the handshake.
    pub async fn connect(io: S, mut config: ClientConfig, name: ServerName<'static>) -> io::Result<Self> {
        let conn = ClientConnection::new(&mut config, name).map_err(tls_error)?;
        let mut stream = Self { io, conn, config, eof: false, closing: false, send_limit: None, applied: 0, held: 0 };
        poll_fn(|cx| stream.poll_handshake(cx)).await?;
        Ok(stream)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> Stream<S, ServerConnection> {
    /// Answer a client over `io` and complete the handshake.
    pub async fn accept(io: S, config: ServerConfig) -> io::Result<Self> {
        let conn = ServerConnection::new(&config).map_err(tls_error)?;
        let mut stream = Self { io, conn, config, eof: false, closing: false, send_limit: None, applied: 0, held: 0 };
        poll_fn(|cx| stream.poll_handshake(cx)).await?;
        Ok(stream)
    }

    /// The name the client asked for (SNI), if it sent one.
    pub fn server_name(&self) -> Option<&str> {
        self.conn.server_name()
    }

    /// Bounds what TLS buffers to send to what `bound.limit` holds whenever it's written to (0: TLS's
    /// own, 64 KiB), read as the session writes, and counts into `bound` what TLS holds
    /// (`crate::live`, the browser protocol's socket).
    pub fn set_send_limit(&mut self, bound: std::sync::Arc<SendBound>) {
        self.send_limit = Some(bound);
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
        let wrote = match self.conn.write_tls(&mut io) {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Poll::Pending,
            result => result,
        };
        if !self.conn.wants_write() {
            self.note_held(None);
        }
        Poll::Ready(wrote)
    }

    /// What TLS holds unwritten: `took` more plaintext taken, or (`None`) none left to write.
    fn note_held(&mut self, took: Option<usize>) {
        let Some(bound) = self.send_limit.as_ref() else { return };
        self.held = took.map_or(0, |n| self.held.saturating_add(n as u64));
        bound.held.store(self.held, std::sync::atomic::Ordering::Relaxed);
        bound.held_max.fetch_max(self.held, std::sync::atomic::Ordering::Relaxed);
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

    /// Follows the send bound as it's set (0: TLS's own).
    fn apply_send_limit(&mut self) {
        let Some(limit) = self.send_limit.as_ref().map(|b| b.limit.load(std::sync::atomic::Ordering::Relaxed)) else { return };
        if limit != self.applied {
            self.applied = limit;
            self.conn.set_buffer_limit((limit > 0).then_some(limit));
        }
    }

    fn poll_write_plain(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        if self.conn.is_handshaking() {
            ready!(self.poll_handshake(cx))?;
        }
        self.apply_send_limit();
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
            self.note_held(Some(took));
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

    /// Records from one end to the other until neither has more to say: the bytes each way (the
    /// server's handshake flight apart from what it sends once its handshake is done: the
    /// tickets), and the client's first flight (its ClientHello), kept to be replayed.
    struct Pumped {
        up: usize,
        down: usize,
        tickets: usize,
        hello: Vec<u8>,
    }

    fn pump(client: &mut ClientConnection, ccfg: &mut ClientConfig, server: &mut ServerConnection, scfg: &mut ServerConfig) -> anyhow::Result<Pumped> {
        let mut p = Pumped { up: 0, down: 0, tickets: 0, hello: Vec::new() };
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
            let handshaking = server.is_handshaking();
            while server.wants_write() {
                server.write_tls(&mut down)?;
            }
            if handshaking {
                p.down += down.len();
            } else {
                p.tickets += down.len();
            }
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
    /// The tickets after either handshake (the same 2, the session sealed in each) aren't
    /// handshake bytes: counted apart.
    /// Stateless: the same ClientHello sent again resumes too, and no early data is offered.
    #[test]
    fn g7_s3_a_ticket_from_one_connection_resumes_the_next() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pki = crate::pki::Pki::open(dir.path())?;
        let (cert, key) = kiln_leaf(&pki, &["kiln.test"])?;
        let server = ServerTls::from_pem(&cert, &key)?;
        let mut ccfg = ClientTls::new(Roots::Pem(pki.ca_pem.clone().into_bytes()), None)?.config()?;
        let (full, ck, sk) = connect(&mut ccfg, &server)?;
        assert_eq!((ck, sk), (Some(rustls::HandshakeKind::Full), Some(rustls::HandshakeKind::Full)));
        assert!(full.tickets > 0, "tickets sent after the handshake");
        let (resumed, ck, sk) = connect(&mut ccfg, &server)?;
        assert_eq!((ck, sk), (Some(rustls::HandshakeKind::Resumed), Some(rustls::HandshakeKind::Resumed)));
        let fall = 1.0 - resumed.down as f64 / full.down as f64;
        assert!(fall >= 0.25, "server-to-client handshake bytes: full {} B, resumed {} B ({:.1}% less; tickets after: {} B, {} B)", full.down, resumed.down, fall * 100.0, full.tickets, resumed.tickets);
        let leaf = server.leaf().map_or(0, <[u8]>::len);
        assert!(full.down.saturating_sub(resumed.down) >= leaf, "the certificate ({leaf} B) isn't sent again: full {} B, resumed {} B", full.down, resumed.down);
        // The third resumes too, with another ticket.
        let (_, ck, _) = connect(&mut ccfg, &server)?;
        assert_eq!(ck, Some(rustls::HandshakeKind::Resumed));
        // The resumed ClientHello again, whole: nothing was used up, so it resumes; 0-RTT is off.
        let mut scfg = server.config()?;
        assert_eq!(scfg.max_early_data_size, 0);
        let mut again = ServerConnection::new(&scfg)?;
        let mut rd = resumed.hello.as_slice();
        while !rd.is_empty() {
            again.read_tls(&mut rd)?;
            again.process_new_packets(&mut scfg)?;
        }
        assert_eq!(again.handshake_kind(), Some(rustls::HandshakeKind::Resumed));
        Ok(())
    }

    /// Goal 7 step 3, lockless (the API runtime's goal 5): a ticket made on one thread resumes on
    /// another, and several resume at once, each on its own thread: the listener's keys are one copy
    /// every connection reads, wherever it runs.
    #[test]
    fn g7_s3_a_ticket_resumes_on_another_thread() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pki = crate::pki::Pki::open(dir.path())?;
        let (cert, key) = kiln_leaf(&pki, &["kiln.test"])?;
        let server = ServerTls::from_pem(&cert, &key)?;
        let roots = ClientTls::new(Roots::Pem(pki.ca_pem.clone().into_bytes()), None)?;
        let here = std::thread::current().id();
        // Clients whose first connection, a full one, is made on this thread.
        let mut clients = Vec::new();
        for _ in 0..4 {
            let mut ccfg = roots.config()?;
            let (_, ck, _) = connect(&mut ccfg, &server)?;
            assert_eq!(ck, Some(rustls::HandshakeKind::Full));
            clients.push(ccfg);
        }
        // Their next connections, at once, each on a thread of its own.
        let threads: Vec<_> = clients
            .into_iter()
            .map(|mut ccfg| {
                let server = server.clone();
                std::thread::spawn(move || -> anyhow::Result<_> {
                    let (_, ck, sk) = connect(&mut ccfg, &server)?;
                    Ok((ck, sk, std::thread::current().id()))
                })
            })
            .collect();
        for t in threads {
            let (ck, sk, on) = t.join().map_err(|_| anyhow::anyhow!("a thread panicked"))??;
            assert_ne!(on, here);
            assert_eq!((ck, sk), (Some(rustls::HandshakeKind::Resumed), Some(rustls::HandshakeKind::Resumed)), "resumed on another thread");
        }
        Ok(())
    }

    /// Goal 7 step 3: a renewed certificate (the same names) keeps the listener's ticket keys, so
    /// tickets from before it resume; one for other names has keys of its own, and they don't.
    /// Either way new connections get the new certificate.
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
        assert!(Arc::ptr_eq(renewed.tickets(), old.tickets()));
        assert_ne!(renewed.leaf(), old.leaf());
        let (_, ck, _) = connect(&mut ccfg, &renewed)?;
        assert_eq!(ck, Some(rustls::HandshakeKind::Resumed), "resumed across the renewal");
        let (cert, key) = kiln_leaf(&pki, &["kiln.test", "other.test"])?;
        let other = ServerTls::from_pem(&cert, &key)?.succeeding(&renewed);
        assert!(!Arc::ptr_eq(other.tickets(), renewed.tickets()));
        let mut client = ClientConnection::new(&mut ccfg, server_name("kiln.test")?)?;
        let mut scfg = other.config()?;
        let mut conn = ServerConnection::new(&scfg)?;
        pump(&mut client, &mut ccfg, &mut conn, &mut scfg)?;
        assert_eq!(client.handshake_kind(), Some(rustls::HandshakeKind::Full));
        assert_eq!(client.peer_certificates().and_then(|c| c.first()).map(|c| c.as_ref()), other.leaf(), "the new certificate");
        Ok(())
    }

    /// Goal 7 step 3, key rotation: a key seals tickets for one lifetime, then a new one takes
    /// over, and the old one still opens its tickets until each is a lifetime old; after that, and
    /// once it's gone, they don't. A ticket a byte off, or from the future, opens nowhere. And over
    /// real handshakes: after a rotation, a ticket from before it resumes.
    #[test]
    fn g7_s3_a_rotated_key_still_opens_its_tickets_until_they_expire() -> anyhow::Result<()> {
        const DAY: u64 = 24 * 60 * 60;
        let t0 = 1_800_000_000;
        let keys = TicketKeys::new_at(Resumption::default(), t0)?;
        assert_eq!(keys.resumption().lifetime, TICKET_LIFETIME);
        let early = keys.seal_at(t0 + 60, b"early").context("sealed")?;
        let late = keys.seal_at(t0 + DAY - 60, b"late").context("sealed")?;
        assert_eq!(keys.open_at(t0 + 120, &early).as_deref(), Some(&b"early"[..]));
        let first = keys.ring.get();
        // A day on: a new key seals, the first one is kept for its tickets.
        let next = keys.seal_at(t0 + DAY, b"next").context("sealed")?;
        let ring = keys.ring.get();
        assert!(!Arc::ptr_eq(&ring.current, &first.current), "a new key at least daily");
        assert!(ring.previous.as_ref().is_some_and(|p| Arc::ptr_eq(p, &first.current)));
        assert_eq!(ring.until, t0 + 2 * DAY);
        assert_ne!(next.get(..KEY_NAME), late.get(..KEY_NAME), "sealed by the new key");
        assert_eq!(keys.open_at(t0 + DAY + 30, &late).as_deref(), Some(&b"late"[..]), "the previous key still opens its tickets");
        assert_eq!(keys.open_at(t0 + DAY + 30, &next).as_deref(), Some(&b"next"[..]));
        // Each ticket for its lifetime, no longer.
        assert!(keys.open_at(t0 + DAY + 60, &early).is_none(), "a day old: expired");
        assert!(keys.open_at(t0 + 2 * DAY - 61, &late).is_some());
        assert!(keys.open_at(t0 + 2 * DAY - 60, &late).is_none(), "a day old: expired");
        // Two days on: the next key; the first is gone, the second kept for its tickets.
        let last = keys.seal_at(t0 + 2 * DAY - 10, b"last").context("sealed")?;
        assert!(keys.seal_at(t0 + 2 * DAY, b"third").is_some());
        let ring = keys.ring.get();
        assert_eq!(ring.previous.as_ref().map(|p| p.name.as_slice()), next.get(..KEY_NAME));
        assert_eq!(keys.open_at(t0 + 2 * DAY + 1, &last).as_deref(), Some(&b"last"[..]));
        assert!(keys.open_at(t0 + 2 * DAY + 1, &next).is_none(), "a day old");
        // Idle for longer than a lifetime: the key before goes with no tickets left to open.
        let _ = keys.seal_at(t0 + 5 * DAY, b"after a while").context("sealed")?;
        assert!(keys.ring.get().previous.is_none());
        // Tampered, truncated, from the future: refused, nothing panics.
        let fresh = keys.seal_at(t0 + 5 * DAY + 1, b"fresh").context("sealed")?;
        for i in 0..fresh.len() {
            let mut bad = fresh.clone();
            if let Some(b) = bad.get_mut(i) {
                *b ^= 1;
            }
            assert!(keys.open_at(t0 + 5 * DAY + 2, &bad).is_none(), "byte {i} changed");
        }
        for n in 0..fresh.len() {
            assert!(keys.open_at(t0 + 5 * DAY + 2, fresh.get(..n).unwrap_or_default()).is_none());
        }
        assert!(keys.open_at(t0 + 5 * DAY, &fresh).is_none(), "issued after now");
        assert_eq!(TicketKeys::new(Resumption { lifetime: Duration::from_secs(30 * DAY), ..Resumption::default() })?.resumption().lifetime, MAX_TICKET_LIFETIME);

        // Real handshakes: tickets from before a rotation resume after it, and the new key seals.
        let dir = tempfile::tempdir()?;
        let pki = crate::pki::Pki::open(dir.path())?;
        let (cert, key) = kiln_leaf(&pki, &["kiln.test"])?;
        let server = ServerTls::from_pem(&cert, &key)?;
        let mut ccfg = ClientTls::new(Roots::Pem(pki.ca_pem.clone().into_bytes()), None)?.config()?;
        connect(&mut ccfg, &server)?;
        let before = server.tickets().ring.get();
        server.tickets().rotate()?;
        let after = server.tickets().ring.get();
        assert!(!Arc::ptr_eq(&before.current, &after.current) && after.previous.as_ref().is_some_and(|p| Arc::ptr_eq(p, &before.current)));
        let (_, ck, sk) = connect(&mut ccfg, &server)?;
        assert_eq!((ck, sk), (Some(rustls::HandshakeKind::Resumed), Some(rustls::HandshakeKind::Resumed)), "the previous key's ticket resumes");
        Ok(())
    }
}
