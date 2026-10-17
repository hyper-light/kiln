//! The front door's HTTPS: TLS terminated here, then the bytes piped both ways to the kiln server
//! (HTTP, or HTTPS trusting kiln's CA), so WebSockets, streamed logs and uploads work unchanged.
//! And plain HTTP on another port, answered with a redirect to it.
//!
//! * **Listening.** `[::]:<port>` accepting IPv4 too, else `0.0.0.0:<port>`; TCP keepalive (60 s)
//!   so phones that leave the Wi-Fi do not hold connections forever. At most [`MAX_CONNECTIONS`]
//!   at once; the TLS handshake gets [`HANDSHAKE`], the upstream connection [`CONNECT`].
//! * **Certificates** are swapped in place (renewal, upload): new connections get the new one,
//!   open ones keep theirs.
//! * **Ending.** Each direction ends on its reader's end and closes its writer (close_notify for
//!   TLS); the connection is done when both are. A stopped listener ends its connections.

use std::future::poll_fn;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll, ready};
use std::time::Duration;

use anyhow::Context as _;
use futures::FutureExt as _;
use futures::io::{AsyncRead, AsyncWrite};
use http::{StatusCode, header};
use parking_lot::RwLock;

use crate::http::io::RtFuturesIo;
use crate::http::server::{self as srv, Request, Response};
use crate::http::tls::{ClientTls, ServerTls, ServerTlsStream, TlsStream};
use crate::rt::Shutdown;

pub const MAX_CONNECTIONS: usize = 512;
pub const HANDSHAKE: Duration = Duration::from_secs(10);
pub const CONNECT: Duration = Duration::from_secs(5);
const BUFFER: usize = 32 * 1024;

/// Where the front door forwards to.
#[derive(Clone)]
pub struct Upstream {
    pub url: String,
    pub host: String,
    pub port: u16,
    /// For an `https://` upstream: kiln's CA and the public roots.
    pub tls: Option<Arc<ClientTls>>,
}

impl std::fmt::Debug for Upstream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Upstream").field("url", &self.url).finish()
    }
}

impl Upstream {
    /// `http://host[:port]` or `https://host[:port]` (trusting `ca_der`, kiln's CA, and the
    /// public roots).
    pub fn parse(url: &str, ca_der: &[u8]) -> anyhow::Result<Self> {
        let uri: http::Uri = url.trim().parse().with_context(|| format!("upstream {url:?}"))?;
        let https = match uri.scheme_str() {
            Some("https") => true,
            Some("http") => false,
            other => anyhow::bail!("upstream {url:?}: scheme {other:?} is neither http nor https"),
        };
        let host = uri.host().filter(|h| !h.is_empty()).with_context(|| format!("upstream {url:?} has no host"))?;
        let host = host.trim_start_matches('[').trim_end_matches(']').to_owned();
        let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
        let tls = if https {
            let roots = if ca_der.is_empty() { crate::http::client::Roots::WebPki } else { crate::http::client::Roots::WebPkiPlusDer(ca_der.to_vec()) };
            Some(Arc::new(ClientTls::new(roots, None)?))
        } else {
            None
        };
        Ok(Self { url: url.trim().trim_end_matches('/').to_owned(), host, port, tls })
    }
}

/// What the HTTPS listener reads, swapped in place while it runs; and what it counts.
#[derive(Default)]
pub struct Shared {
    pub tls: RwLock<Option<ServerTls>>,
    pub upstream: RwLock<Option<Upstream>>,
    /// Connections that completed the TLS handshake.
    pub served: AtomicU64,
    pub open: AtomicUsize,
}

/// A running listener: it stops, with its connections, when dropped.
pub struct Listening {
    stop: Shutdown,
    pub bound: SocketAddr,
}

impl Drop for Listening {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

/// A TCP listener on `port` for every address: IPv6 accepting IPv4 too, else IPv4 only.
fn bind(port: u16) -> io::Result<(std::net::TcpListener, SocketAddr)> {
    use socket2::{Domain, Protocol, Socket, TcpKeepalive, Type};
    let open = |addr: SocketAddr| -> io::Result<std::net::TcpListener> {
        let s = Socket::new(if addr.is_ipv6() { Domain::IPV6 } else { Domain::IPV4 }, Type::STREAM, Some(Protocol::TCP))?;
        if addr.is_ipv6() {
            s.set_only_v6(false)?;
        }
        s.set_reuse_address(true)?;
        // Accepted connections inherit it.
        let _ = s.set_tcp_keepalive(&TcpKeepalive::new().with_time(Duration::from_secs(60)));
        s.bind(&addr.into())?;
        s.listen(256)?;
        Ok(s.into())
    };
    let v6 = SocketAddr::from((Ipv6Addr::UNSPECIFIED, port));
    match open(v6) {
        Ok(l) => Ok((l, v6)),
        // No IPv6 here: IPv4 alone. Permission and "in use" would be the same for IPv4.
        Err(e) if !matches!(e.kind(), io::ErrorKind::PermissionDenied | io::ErrorKind::AddrInUse) => {
            let v4 = SocketAddr::from((Ipv4Addr::UNSPECIFIED, port));
            open(v4).map(|l| (l, v4))
        }
        Err(e) => Err(e),
    }
}

fn adopt(l: std::net::TcpListener) -> io::Result<hyper_rt::tcp::TcpListener> {
    hyper_rt::tcp::TcpListener::from_owned(hyper_rt::tcp::OwnedStream::from(l), 256).map_err(crate::rt::to_io)
}

/// Serve HTTPS on `port` with what `shared` holds.
pub fn listen_https(port: u16, shared: &Arc<Shared>) -> io::Result<Listening> {
    let (std_listener, bound) = bind(port)?;
    let stop = Shutdown::new();
    let (sh, sd) = (Arc::clone(shared), stop.clone());
    crate::rt::spawn(async move {
        let listener = match adopt(std_listener) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %e, "front door: cannot hand the HTTPS listener to the runtime");
                return;
            }
        };
        loop {
            let accepted = futures::select_biased! {
                () = sd.cancelled().fuse() => return,
                r = listener.accept().fuse() => r,
            };
            let stream = match accepted {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(error = %e, "front door: accept failed");
                    crate::rt::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            if sh.open.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
                continue;
            }
            sh.open.fetch_add(1, Ordering::Relaxed);
            let (sh, sd) = (Arc::clone(&sh), sd.clone());
            crate::rt::spawn_spread(async move {
                let _ = crate::discovery::until_stopped(&sd, async {
                    if let Err(e) = proxy(stream, &sh).await {
                        tracing::debug!(error = %format!("{e:#}"), "front door connection ended");
                    }
                })
                .await;
                sh.open.fetch_sub(1, Ordering::Relaxed);
            });
        }
    });
    Ok(Listening { stop, bound })
}

/// One connection: the TLS handshake, the upstream connection, then the pipe.
async fn proxy(tcp: hyper_rt::tcp::TcpStream, shared: &Shared) -> anyhow::Result<()> {
    let tls = shared.tls.read().clone().context("no certificate yet")?;
    let upstream = shared.upstream.read().clone().context("no upstream")?;
    let client = crate::rt::timeout(HANDSHAKE, ServerTlsStream::accept(RtFuturesIo::new(tcp), tls.config()?))
        .await
        .context("TLS handshake timed out")?
        .context("TLS handshake")?;
    shared.served.fetch_add(1, Ordering::Relaxed);
    let up = crate::rt::timeout(CONNECT, crate::http::io::connect(&upstream.host, upstream.port))
        .await
        .with_context(|| format!("{} did not answer", upstream.url))??;
    match &upstream.tls {
        None => pipe(client, RtFuturesIo::new(up)).await?,
        Some(ct) => {
            let name = crate::http::tls::server_name(&upstream.host)?;
            let up = crate::rt::timeout(HANDSHAKE, TlsStream::connect(RtFuturesIo::new(up), ct.config()?, name))
                .await
                .context("TLS to the upstream timed out")?
                .with_context(|| format!("TLS to {}", upstream.url))?;
            pipe(client, up).await?
        }
    };
    Ok(())
}

/// One direction of a pipe.
struct Half {
    buf: Box<[u8]>,
    start: usize,
    end: usize,
    eof: bool,
    done: bool,
    total: u64,
}

impl Half {
    fn new() -> Self {
        Self { buf: vec![0u8; BUFFER].into_boxed_slice(), start: 0, end: 0, eof: false, done: false, total: 0 }
    }

    /// Copy from `from` to `to` until `from` ends; then close `to`'s writing side.
    fn poll<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(&mut self, cx: &mut Context<'_>, from: &mut R, to: &mut W) -> Poll<io::Result<u64>> {
        loop {
            if self.done {
                return Poll::Ready(Ok(self.total));
            }
            while self.start < self.end {
                let chunk = self.buf.get(self.start..self.end).unwrap_or_default();
                let n = ready!(Pin::new(&mut *to).poll_write(cx, chunk))?;
                if n == 0 {
                    return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
                }
                self.start += n;
                self.total = self.total.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
            }
            if self.eof {
                ready!(Pin::new(&mut *to).poll_close(cx))?;
                self.done = true;
                continue;
            }
            self.start = 0;
            self.end = 0;
            match Pin::new(&mut *from).poll_read(cx, &mut self.buf) {
                Poll::Ready(Ok(0)) => self.eof = true,
                Poll::Ready(Ok(n)) => self.end = n,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => {
                    // Nothing more to read for now: what was written goes out before waiting.
                    ready!(Pin::new(&mut *to).poll_flush(cx))?;
                    return Poll::Pending;
                }
            }
        }
    }
}

/// Copy bytes both ways between `a` and `b` until both directions have ended: the bytes from
/// `a` to `b` and from `b` to `a`. Either side failing ends both.
pub async fn pipe<A, B>(mut a: A, mut b: B) -> io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let mut ab = Half::new();
    let mut ba = Half::new();
    poll_fn(|cx| {
        let there = ab.poll(cx, &mut a, &mut b)?;
        let back = ba.poll(cx, &mut b, &mut a)?;
        match (there, back) {
            (Poll::Ready(x), Poll::Ready(y)) => Poll::Ready(Ok((x, y))),
            _ => Poll::Pending,
        }
    })
    .await
}

// ------------------------------------------------------------------------------ plain HTTP

/// Where the plain-HTTP redirect points.
#[derive(Default)]
pub struct Redirect {
    /// Names it redirects to as asked (`Host`); any other goes to `primary`.
    pub names: RwLock<Vec<String>>,
    pub primary: RwLock<String>,
    pub https_port: AtomicU16,
}

impl Redirect {
    fn location(&self, host: Option<&str>, path: &str) -> String {
        let host = host.map(strip_port).unwrap_or_default();
        let ours = !host.is_empty() && (crate::pki::covers(&self.names.read(), &host) || host.trim_matches(['[', ']']).parse::<IpAddr>().is_ok());
        let host = if ours { host } else { self.primary.read().clone() };
        let port = match self.https_port.load(Ordering::Relaxed) {
            443 => String::new(),
            p => format!(":{p}"),
        };
        format!("https://{host}{port}{path}")
    }
}

/// `host[:port]` without the port (IPv6 literals keep their brackets).
fn strip_port(h: &str) -> String {
    let h = h.trim();
    if h.starts_with('[') {
        return h.split_inclusive(']').next().unwrap_or(h).to_owned();
    }
    h.rsplit_once(':').filter(|(_, p)| p.bytes().all(|b| b.is_ascii_digit())).map_or(h, |(n, _)| n).to_ascii_lowercase()
}

struct RedirectHandler(Arc<Redirect>);

impl srv::Handler for RedirectHandler {
    async fn handle(self: Arc<Self>, req: Request) -> Response {
        let host = srv::header_str(&req, &header::HOST);
        let path = req.uri().path_and_query().map_or("/", http::uri::PathAndQuery::as_str);
        let to = self.0.location(host, path);
        let body = format!("<!doctype html><title>kiln</title><p>kiln is at <a href=\"{0}\">{0}</a></p>\n", html_escape(&to));
        let r = srv::response(StatusCode::TEMPORARY_REDIRECT, "text/html; charset=utf-8", srv::full(body));
        srv::with_header(r, header::LOCATION, &to)
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Plain HTTP on `port`, every request redirected to the HTTPS front door.
pub fn listen_redirect(port: u16, redirect: &Arc<Redirect>) -> io::Result<Listening> {
    let (std_listener, bound) = bind(port)?;
    let stop = Shutdown::new();
    let (handler, sd) = (Arc::new(RedirectHandler(Arc::clone(redirect))), stop.clone());
    crate::rt::spawn(async move {
        let listener = match adopt(std_listener) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %e, "front door: cannot hand the HTTP listener to the runtime");
                return;
            }
        };
        let open = Arc::new(AtomicUsize::new(0));
        loop {
            let accepted = futures::select_biased! {
                () = sd.cancelled().fuse() => return,
                r = listener.accept().fuse() => r,
            };
            let Ok(stream) = accepted else {
                crate::rt::sleep(Duration::from_millis(100)).await;
                continue;
            };
            if open.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
                continue;
            }
            open.fetch_add(1, Ordering::Relaxed);
            let (h, sd, n) = (Arc::clone(&handler), sd.clone(), Arc::clone(&open));
            crate::rt::spawn_spread(async move {
                let remote = stream.peer_addr().ok();
                let _ = crate::discovery::until_stopped(&sd, srv::serve_connection(crate::http::io::RtIo::new(stream), h, remote)).await;
                n.fetch_sub(1, Ordering::Relaxed);
            });
        }
    });
    Ok(Listening { stop, bound })
}

#[cfg(test)]
mod tests {
    use futures::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;

    #[test]
    fn redirects_keep_the_name_asked_when_it_is_ours() {
        let r = Redirect::default();
        *r.names.write() = vec!["kiln.home".into(), "*.kiln.home".into(), "kiln.local".into()];
        *r.primary.write() = "kiln.home".into();
        r.https_port.store(443, Ordering::Relaxed);
        assert_eq!(r.location(Some("kiln.home"), "/runs?x=1"), "https://kiln.home/runs?x=1");
        assert_eq!(r.location(Some("KILN.local:80"), "/"), "https://kiln.local/");
        assert_eq!(r.location(Some("ci.kiln.home:80"), "/"), "https://ci.kiln.home/");
        assert_eq!(r.location(Some("evil.example"), "/x"), "https://kiln.home/x");
        assert_eq!(r.location(None, "/"), "https://kiln.home/");
        assert_eq!(r.location(Some("192.168.1.77"), "/"), "https://192.168.1.77/");
        r.https_port.store(8443, Ordering::Relaxed);
        assert_eq!(r.location(Some("[fd00::77]:8080"), "/"), "https://[fd00::77]:8443/");
        assert_eq!(strip_port("kiln.home:80"), "kiln.home");
        assert_eq!(html_escape("\"<a>&"), "&quot;&lt;a&gt;&amp;");
    }

    #[test]
    fn upstreams_parse() -> anyhow::Result<()> {
        let u = Upstream::parse("http://192.168.1.77:7070/", &[])?;
        assert_eq!((u.host.as_str(), u.port, u.tls.is_some(), u.url.as_str()), ("192.168.1.77", 7070, false, "http://192.168.1.77:7070"));
        let u = Upstream::parse("https://kiln.kiln.svc", &[])?;
        assert_eq!((u.port, u.tls.is_some()), (443, true));
        let u = Upstream::parse("http://[::1]:7070", &[])?;
        assert_eq!(u.host, "::1");
        assert!(Upstream::parse("ftp://x", &[]).is_err());
        assert!(Upstream::parse("nonsense", &[]).is_err());
        Ok(())
    }

    /// Bytes go both ways through the pipe, ends travel, and the totals add up.
    #[test]
    fn the_pipe_carries_both_directions_and_their_ends() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let listener = hyper_rt::tcp::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), 8).map_err(|e| anyhow::anyhow!("{e}"))?;
            let echo_addr = listener.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
            // An "upstream" that answers each byte it reads with the same byte, uppercased.
            crate::rt::spawn(async move {
                if let Ok(s) = listener.accept().await {
                    let mut io = RtFuturesIo::new(s);
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = io.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        let up: Vec<u8> = buf.get(..n).unwrap_or_default().iter().map(u8::to_ascii_uppercase).collect();
                        if io.write_all(&up).await.is_err() {
                            break;
                        }
                    }
                    let _ = io.close().await;
                }
            });
            let front = hyper_rt::tcp::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), 8).map_err(|e| anyhow::anyhow!("{e}"))?;
            let front_addr = front.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
            let piped = crate::rt::spawn(async move {
                let client = front.accept().await.map_err(|e| anyhow::anyhow!("{e}"))?;
                let up = hyper_rt::tcp::TcpStream::connect(echo_addr).await.map_err(|e| anyhow::anyhow!("{e}"))?;
                Ok::<_, anyhow::Error>(pipe(RtFuturesIo::new(client), RtFuturesIo::new(up)).await?)
            });
            let c = RtFuturesIo::new(hyper_rt::tcp::TcpStream::connect(front_addr).await.map_err(|e| anyhow::anyhow!("{e}"))?);
            let big: Vec<u8> = (0..200_000u32).map(|i| b'a' + u8::try_from(i % 26).unwrap_or(0)).collect();
            let (mut rd, mut wr) = c.split();
            let write = async {
                wr.write_all(&big).await?;
                wr.close().await?;
                Ok::<_, io::Error>(())
            };
            let read = async {
                let mut back = Vec::new();
                rd.read_to_end(&mut back).await?;
                Ok::<_, io::Error>(back)
            };
            let (written, back) = futures::join!(write, read);
            written?;
            let back = back?;
            assert_eq!(back.len(), big.len());
            assert!(back.iter().zip(&big).all(|(b, a)| *b == a.to_ascii_uppercase()));
            let (there, here) = piped.await.map_err(|_| anyhow::anyhow!("pipe task"))??;
            assert_eq!((there, here), (200_000, 200_000));
            Ok(())
        })
    }
}
