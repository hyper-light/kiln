//! HTTPS client TLS on hyper-tls (hyper-raft's rustls fork, reached as `hyper_quic::rustls`),
//! driven over any futures-io transport. No tokio, no tokio-rustls.
//!
//! hyper-tls connections hold no configuration: every call that may advance the state machine
//! (`process_new_packets(&mut config)`) borrows it, for the connection's whole life (a TLS 1.3
//! client files NewSessionTickets into it after the handshake). So each connection owns its own
//! [`ClientConfig`], built from a shared root store ([`ClientTls::config`]).

use std::future::poll_fn;
use std::io::{self, BufRead as _, Read as _, Write as _};
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use anyhow::Context as _;
use futures::io::{AsyncRead, AsyncWrite};
use hyper_quic::rustls::crypto::CryptoProvider;
use hyper_quic::rustls::pki_types::pem::PemObject as _;
use hyper_quic::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use hyper_quic::rustls::{self, ClientConfig, ClientConnection, RootCertStore};

use super::client::{Identity, Roots};

/// Providers for HTTPS to arbitrary servers: aws-lc-rs's defaults (TLS 1.3 and 1.2, X25519MLKEM768,
/// X25519, P-256, P-384). Not hyper-quic's node provider, which is post-quantum only and is refused
/// by classical servers such as GitHub.
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
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(config)
    }
}

/// The TLS name for a URL host: a DNS name is sent as SNI; an IP literal is matched against IP SANs
/// and never sent as SNI. IPv6 brackets are stripped.
pub fn server_name(host: &str) -> anyhow::Result<ServerName<'static>> {
    let bare = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    ServerName::try_from(bare.to_owned()).map_err(|_| anyhow::anyhow!("invalid TLS server name {host:?}"))
}

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

/// A client TLS session over the transport `S`, owning its configuration.
pub struct TlsStream<S> {
    io: S,
    conn: ClientConnection,
    config: ClientConfig,
    eof: bool,
    closing: bool,
}

impl<S: AsyncRead + AsyncWrite + Unpin> TlsStream<S> {
    /// Start a connection to `name` over `io` and complete the handshake.
    pub async fn connect(io: S, mut config: ClientConfig, name: ServerName<'static>) -> io::Result<Self> {
        let conn = ClientConnection::new(&mut config, name).map_err(tls_error)?;
        let mut stream = Self { io, conn, config, eof: false, closing: false };
        poll_fn(|cx| stream.poll_handshake(cx)).await?;
        Ok(stream)
    }

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

impl<S: AsyncRead + AsyncWrite + Unpin> hyper::rt::Read for TlsStream<S> {
    /// Straight from hyper-tls's plaintext buffer into hyper's; nothing filled means end of stream.
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, mut buf: hyper::rt::ReadBufCursor<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.poll_plaintext(cx))?;
        let mut reader = this.conn.reader();
        let chunk = reader.fill_buf()?;
        let n = chunk.len().min(buf.remaining());
        buf.put_slice(chunk.get(..n).unwrap_or_default());
        reader.consume(n);
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> hyper::rt::Write for TlsStream<S> {
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

/// Plaintext through `std::io::Read`, for code that wants a reader (unused by hyper).
impl<S: AsyncRead + AsyncWrite + Unpin> TlsStream<S> {
    pub async fn read_some(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        poll_fn(|cx| {
            ready!(self.poll_plaintext(cx))?;
            Poll::Ready(self.conn.reader().read(buf))
        })
        .await
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
}
