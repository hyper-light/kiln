//! TLS on hyper-tls (hyper-raft's rustls fork, reached as `hyper_quic::rustls`), driven over any
//! futures-io transport, for both ends: HTTPS clients ([`ClientTls`], [`TlsStream`]) and servers
//! ([`ServerTls`], [`ServerTlsStream`]: the server's own listener with `server.https`, and a
//! runner's front door). No tokio, no tokio-rustls.
//!
//! hyper-tls connections hold no configuration: every call that may advance the state machine
//! (`process_new_packets(&mut config)`) borrows it, for the connection's whole life (a TLS 1.3
//! client files NewSessionTickets into it after the handshake). So each connection owns its own
//! configuration, built from shared material: a root store ([`ClientTls::config`]), or a
//! certificate and its key, parsed once ([`ServerTls::config`]).

use std::future::poll_fn;
use std::io::{self, BufRead as _, Read as _, Write as _};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};

use anyhow::Context as _;
use futures::io::{AsyncRead, AsyncWrite};
use hyper_quic::rustls::crypto::CryptoProvider;
use hyper_quic::rustls::pki_types::pem::PemObject as _;
use hyper_quic::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use hyper_quic::rustls::server::{ClientHello, ResolvesServerCert};
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
            Roots::WebPki => store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
            Roots::Pem(pem) => {
                for cert in CertificateDer::pem_slice_iter(&pem) {
                    store.add(cert.context("CA PEM")?).context("CA certificate")?;
                }
            }
            Roots::WebPkiPlusDer(der) => {
                store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
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

/// A server's certificate chain and key, parsed (and checked to belong together) once; each
/// connection's configuration is made from them ([`Self::config`]). Cheap to clone.
#[derive(Clone)]
pub struct ServerTls {
    key: Arc<CertifiedKey>,
}

impl std::fmt::Debug for ServerTls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerTls").field("chain", &self.key.cert.len()).field("key", &"<redacted>").finish()
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
    /// leaf's. Errors never quote the key.
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
        let tls = Self { key: Arc::new(key) };
        tls.config()?;
        Ok(tls)
    }

    /// The leaf certificate (DER).
    pub fn leaf(&self) -> Option<&[u8]> {
        self.key.cert.first().map(|c| c.as_ref())
    }

    /// A fresh configuration for one connection (TLS 1.3 and 1.2, ALPN http/1.1, no client
    /// certificates).
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
/// ([`TlsStream`]) or a server's ([`ServerTlsStream`]). Plaintext through `hyper::rt` IO (for
/// hyper) and futures-io (for anything else, such as a byte pipe).
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

impl<S: AsyncRead + AsyncWrite + Unpin, C: Side + Unpin> hyper::rt::Read for Stream<S, C>
where
    C::Config: Unpin,
{
    /// Straight from hyper-tls's plaintext buffer into hyper's; nothing filled means end of stream.
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, mut buf: hyper::rt::ReadBufCursor<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.poll_plaintext(cx))?;
        let mut reader = this.conn.reader();
        let chunk = match reader.fill_buf() {
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Poll::Ready(Ok(())),
            other => other?,
        };
        let n = chunk.len().min(buf.remaining());
        buf.put_slice(chunk.get(..n).unwrap_or_default());
        reader.consume(n);
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin, C: Side + Unpin> hyper::rt::Write for Stream<S, C>
where
    C::Config: Unpin,
{
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        self.get_mut().poll_write_plain(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().poll_flush_plain(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().poll_close_plain(cx)
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

/// Plaintext through `std::io::Read`, for code that wants a reader (unused by hyper).
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

/// futures-io as hyper IO: one copy per read through a scratch buffer (safe code; hyper's
/// uninitialised read cursor is only reachable through `unsafe`).
pub struct HyperIo<S> {
    io: S,
    scratch: Box<[u8]>,
}

impl<S> HyperIo<S> {
    pub fn new(io: S) -> Self {
        Self { io, scratch: vec![0u8; 16 * 1024].into_boxed_slice() }
    }
}

impl<S: AsyncRead + Unpin> hyper::rt::Read for HyperIo<S> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, mut buf: hyper::rt::ReadBufCursor<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let want = buf.remaining().min(this.scratch.len());
        let Some(scratch) = this.scratch.get_mut(..want) else {
            return Poll::Ready(Ok(()));
        };
        if want == 0 {
            return Poll::Ready(Ok(()));
        }
        let n = ready!(Pin::new(&mut this.io).poll_read(cx, scratch))?;
        if let Some(read) = scratch.get(..n) {
            buf.put_slice(read);
        }
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncWrite + Unpin> hyper::rt::Write for HyperIo<S> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().io).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
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
}
