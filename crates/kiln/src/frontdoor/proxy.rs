//! The front door's HTTPS: TLS terminated here, then the bytes piped both ways to the kiln server
//! (HTTP, or HTTPS trusting kiln's CA), so WebSockets, streamed logs and uploads work unchanged.
//! HTTP/2 passes through the same way, offered to browsers while the upstream is found to speak
//! it ([`speaks_h2`], asked every [`PROBE_EVERY`]): a browser then makes one connection for a
//! page's requests, not six.
//! And plain HTTP on another port, answered with a redirect to it, except for what a device needs
//! before it can trust that HTTPS: kiln's CA (`/ca.mobileconfig`, `/ca.crt`, `/ca.pem`), and, at an
//! address rather than a name, a short page saying how to trust it.
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
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll, ready};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use futures::FutureExt as _;
use futures::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use http::{StatusCode, header};
use parking_lot::RwLock;

use crate::http::io::RtFuturesIo;
use crate::http::server::{self as srv, Request, Response};
use crate::http::tls::{ClientTls, H2, HTTP11, ServerTls, ServerTlsStream, TlsStream};
use crate::rt::Shutdown;

pub const MAX_CONNECTIONS: usize = 512;
pub const HANDSHAKE: Duration = Duration::from_secs(10);
pub const CONNECT: Duration = Duration::from_secs(5);
const BUFFER: usize = 32 * 1024;
/// How often the front door asks its upstream whether it speaks HTTP/2, and how long it waits.
pub const PROBE_EVERY: Duration = Duration::from_secs(30);
const PROBE_WAIT: Duration = Duration::from_secs(5);

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
    /// The upstream speaks HTTP/2 (last asked): browsers are offered it.
    pub h2: AtomicBool,
    /// The clusters this one is peered with: a connection for one's address (`peer-<slug>.…`)
    /// is forwarded from here, not passed to the upstream.
    pub peers: RwLock<Option<Arc<crate::peers::forward::Forwarder>>>,
    /// While this runner holds a replica of kiln's records: the handler that serves requests one
    /// by one (kept across connections, with what it learned of whose credentials are whose).
    pub door: RwLock<Option<Arc<super::replica::Door>>>,
}

/// The handler for requests one by one while this runner holds a replica of kiln's records
/// (docs/p2p.md, P2); `None`: none here, and connections are piped to the upstream whole.
fn door_for(shared: &Shared, upstream: &Upstream) -> Option<Arc<super::replica::Door>> {
    let Some(learner) = crate::store::learner::global() else {
        // None now: the one before lets its replica go.
        shared.door.write().take();
        return None;
    };
    if let Some(d) = shared.door.read().as_ref().filter(|d| d.serves(&learner, upstream)) {
        return Some(Arc::clone(d));
    }
    let d = Arc::new(super::replica::Door::new(upstream.clone(), learner).ok()?);
    *shared.door.write() = Some(Arc::clone(&d));
    Some(d)
}

/// Serves one peered cluster's address on a front-door connection.
struct PeerDoor {
    forwarder: Arc<crate::peers::forward::Forwarder>,
    slug: String,
}

impl srv::Handler for PeerDoor {
    async fn handle(self: Arc<Self>, req: Request) -> Response {
        self.forwarder.serve(&self.slug, req, true).await
    }
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
    crate::rt::spawn(follow_h2(Arc::clone(shared), stop.clone()));
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

/// Keeps [`Shared::h2`] saying whether the upstream speaks HTTP/2: asked at once when the upstream
/// changes (not offered until it answers), and every [`PROBE_EVERY`] (it may be upgraded, or
/// rolled back), until `stop`.
async fn follow_h2(shared: Arc<Shared>, stop: Shutdown) {
    let mut asked: Option<(String, Instant)> = None;
    loop {
        let upstream = shared.upstream.read().clone();
        match upstream {
            None => {
                shared.h2.store(false, Ordering::Relaxed);
                asked = None;
            }
            Some(u) => {
                let moved = asked.as_ref().is_some_and(|(url, _)| *url != u.url);
                if moved {
                    shared.h2.store(false, Ordering::Relaxed);
                }
                if moved || asked.as_ref().is_none_or(|(_, at)| at.elapsed() >= PROBE_EVERY) {
                    let h2 = speaks_h2(&u).await;
                    if shared.h2.swap(h2, Ordering::Relaxed) != h2 {
                        tracing::info!(upstream = %u.url, h2, "front door: HTTP/2 to browsers {}", if h2 { "on" } else { "off" });
                    }
                    asked = Some((u.url, Instant::now()));
                }
            }
        }
        if !stop.sleep(Duration::from_secs(1)).await {
            return;
        }
    }
}

/// Whether `upstream` speaks HTTP/2. Over TLS: whether it agrees to `h2`. In the clear: whether it
/// answers HTTP/2's preface with its settings (a server speaking only HTTP/1.1 answers with an
/// error, or closes). Not within [`PROBE_WAIT`]: no.
pub async fn speaks_h2(upstream: &Upstream) -> bool {
    let ask = async {
        let tcp = crate::http::io::connect(&upstream.host, upstream.port).await?;
        match &upstream.tls {
            Some(ct) => {
                let name = crate::http::tls::server_name(&upstream.host)?;
                let s = TlsStream::connect(RtFuturesIo::new(tcp), ct.config_alpn(&[H2, HTTP11])?, name).await?;
                Ok::<_, anyhow::Error>(s.alpn_protocol() == Some(H2))
            }
            None => {
                let mut io = RtFuturesIo::new(tcp);
                io.write_all(srv::H2_PREFACE).await?;
                // An empty SETTINGS frame: length 0, type 4, no flags, stream 0.
                io.write_all(&[0, 0, 0, 4, 0, 0, 0, 0, 0]).await?;
                io.flush().await?;
                let mut frame = [0u8; 9];
                io.read_exact(&mut frame).await?;
                // The server's SETTINGS (type 4, on stream 0, not an acknowledgement) come first.
                Ok(matches!(frame, [_, _, _, 4, flags, 0, 0, 0, 0] if flags & 1 == 0))
            }
        }
    };
    matches!(crate::rt::timeout(PROBE_WAIT, ask).await, Ok(Ok(true)))
}

/// One connection: the TLS handshake, the upstream connection, then the pipe.
async fn proxy(tcp: hyper_rt::tcp::TcpStream, shared: &Shared) -> anyhow::Result<()> {
    let remote = tcp.peer_addr().ok();
    let tls = shared.tls.read().clone().context("no certificate yet")?;
    let upstream = shared.upstream.read().clone().context("no upstream")?;
    let door = door_for(shared, &upstream);
    // Served one by one, the browser's protocol is the front door's own: HTTP/2 whatever the
    // upstream speaks.
    let offer: &[&[u8]] = if door.is_some() || shared.h2.load(Ordering::Relaxed) { &[H2, HTTP11] } else { &[HTTP11] };
    let client = crate::rt::timeout(HANDSHAKE, ServerTlsStream::accept(RtFuturesIo::new(tcp), tls.config_alpn(offer)?))
        .await
        .context("TLS handshake timed out")?
        .context("TLS handshake")?;
    // The browser's protocol passes through as it is: HTTP/2 needs an upstream that speaks it.
    let h2 = client.alpn_protocol() == Some(H2);
    shared.served.fetch_add(1, Ordering::Relaxed);
    // A peered cluster's address: forwarded from this machine, request by request.
    let peer = client.server_name().map(str::to_ascii_lowercase).and_then(|sni| {
        let f = shared.peers.read().clone()?;
        let slug = f.slug_of_host(&sni)?.to_owned();
        Some(Arc::new(PeerDoor { forwarder: f, slug }))
    });
    if let Some(door) = peer {
        if h2 {
            srv::serve_h2(client, bytes::Bytes::new(), door, remote, true).await;
        } else {
            srv::serve_connection_with(client, door, remote, true).await;
        }
        return Ok(());
    }
    // A replica here: the owner's reads it holds are answered from it, the rest forwarded.
    if let Some(door) = door {
        if h2 {
            srv::serve_h2(client, bytes::Bytes::new(), door, remote, true).await;
        } else {
            srv::serve_connection_with(client, door, remote, true).await;
        }
        return Ok(());
    }
    let up = crate::rt::timeout(CONNECT, crate::http::io::connect(&upstream.host, upstream.port))
        .await
        .with_context(|| format!("{} did not answer", upstream.url))??;
    match &upstream.tls {
        None => pipe(client, RtFuturesIo::new(up)).await?,
        Some(ct) => {
            let name = crate::http::tls::server_name(&upstream.host)?;
            let ask: &[&[u8]] = if h2 { &[H2, HTTP11] } else { &[HTTP11] };
            let up = crate::rt::timeout(HANDSHAKE, TlsStream::connect(RtFuturesIo::new(up), ct.config_alpn(ask)?, name))
                .await
                .context("TLS to the upstream timed out")?
                .with_context(|| format!("TLS to {}", upstream.url))?;
            if h2 && up.alpn_protocol() != Some(H2) {
                // It no longer speaks HTTP/2: this connection can't be carried; the next get HTTP/1.1.
                shared.h2.store(false, Ordering::Relaxed);
                anyhow::bail!("{} no longer speaks HTTP/2", upstream.url);
            }
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

/// Where the plain-HTTP redirect points, and the CA it hands out.
#[derive(Default)]
pub struct Redirect {
    /// Names it redirects to as asked (`Host`); any other goes to `primary`.
    pub names: RwLock<Vec<String>>,
    pub primary: RwLock<String>,
    pub https_port: AtomicU16,
    /// kiln's CA certificate (DER), handed out over plain HTTP: a device can't trust the front
    /// door's HTTPS before it has it, and a `.dev` name can't even be opened until then.
    pub ca_der: RwLock<Vec<u8>>,
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

/// `der` as PEM.
fn pem_of(der: &[u8]) -> String {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut out = String::from("-----BEGIN CERTIFICATE-----\n");
    for line in b64.as_bytes().chunks(64) {
        out.push_str(&String::from_utf8_lossy(line));
        out.push('\n');
    }
    out.push_str("-----END CERTIFICATE-----\n");
    out
}

impl Redirect {
    /// kiln's CA as a file a device installs: the Apple profile, the certificate (DER), or PEM.
    fn ca_file(&self, path: &str) -> Option<Response> {
        let der = self.ca_der.read().clone();
        if der.is_empty() {
            return None;
        }
        let (mime, file, body): (&str, &str, Vec<u8>) = match path {
            "/ca.mobileconfig" => ("application/x-apple-aspen-config", "kiln-ca.mobileconfig", super::mobileconfig::profile(&der).into_bytes()),
            "/ca.crt" => ("application/x-x509-ca-cert", "kiln-ca.crt", der),
            "/ca.pem" => ("application/x-pem-file", "kiln-ca.pem", pem_of(&der).into_bytes()),
            _ => return None,
        };
        let r = srv::response(StatusCode::OK, mime, srv::full(body));
        let r = srv::with_header(r, header::CONTENT_DISPOSITION, &format!("attachment; filename=\"{file}\""));
        Some(srv::with_header(r, header::CACHE_CONTROL, "no-cache"))
    }

    /// What a device that opens the front door by its address sees: how to trust kiln's CA here,
    /// and where kiln is once it does.
    fn trust_page(&self) -> String {
        let der = self.ca_der.read().clone();
        let fingerprint = crate::util::sha256_hex(&der).to_ascii_uppercase().as_bytes().chunks(2).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join(":");
        let primary = self.primary.read().clone();
        let port = match self.https_port.load(Ordering::Relaxed) {
            443 => String::new(),
            p => format!(":{p}"),
        };
        let kiln = html_escape(&format!("https://{primary}{port}/"));
        let name = html_escape(&primary);
        format!(
            r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Trust kiln on this device</title>
<style>
:root{{color-scheme:dark}}body{{margin:0;background:#08090a;color:#d6d7db;font:15px/1.55 system-ui,-apple-system,sans-serif}}
main{{max-width:560px;margin:0 auto;padding:28px 20px 40px}}h1{{margin:0 0 6px;font-size:22px;font-weight:600;color:#ededee}}
p{{margin:0 0 14px}}.k{{margin:22px 0 6px;color:#8f929b;font:11px/1.4 ui-monospace,monospace;letter-spacing:.1em;text-transform:uppercase}}
ol{{margin:0 0 6px;padding-left:20px}}li{{margin:0 0 6px}}a{{color:#d1c6e6}}.go{{display:inline-block;margin:6px 0 2px;padding:11px 18px;border:1px solid #3a3c44;border-radius:999px;color:#ededee;text-decoration:none}}
.note{{color:#8f929b;font-size:13px}}code{{font:12px ui-monospace,monospace;color:#cdc4e0;overflow-wrap:anywhere}}
</style></head><body><main>
<h1>Trust kiln on this device</h1>
<p>{name} uses kiln&rsquo;s own certificate. Trust it once here and <a href="{kiln}">{name}</a> opens without warnings, on every runner and after every renewal.</p>
<p class="k">iPhone, iPad</p>
<ol>
<li>In <b>Safari</b> (Chrome can&rsquo;t install it): <a href="/ca.mobileconfig">install the profile</a>, then Allow.</li>
<li>Settings &rarr; <b>Profile Downloaded</b> &rarr; Install. &ldquo;Not Signed&rdquo; is expected.</li>
<li>Settings &rarr; General &rarr; About &rarr; Certificate Trust Settings &rarr; turn on <b>kiln CA</b>.</li>
</ol>
<p class="note">If Safari then can&rsquo;t find {name}: iCloud Private Relay sends Safari&rsquo;s lookups past your network. Use Chrome for it, or turn Private Relay off.</p>
<p class="k">Mac</p>
<p><a href="/ca.mobileconfig">Install the profile</a>, then System Settings &rarr; General &rarr; Device Management &rarr; kiln CA &rarr; Install.</p>
<p class="k">Android, Windows, Linux</p>
<p><a href="/ca.crt">Download the certificate</a> and install it as a trusted root (CA) certificate.</p>
<p class="k">Then</p>
<p><a class="go" href="{kiln}">Open {name}</a></p>
<p class="note">Check it&rsquo;s kiln&rsquo;s: SHA-256 <code>{fingerprint}</code></p>
</main></body></html>
"#
        )
    }
}

/// Whether `host` (a `Host` header) is an address rather than a name.
fn is_address(host: Option<&str>) -> bool {
    host.map(strip_port).is_none_or(|h| h.is_empty() || h.trim_matches(['[', ']']).parse::<IpAddr>().is_ok())
}

struct RedirectHandler(Arc<Redirect>);

impl srv::Handler for RedirectHandler {
    async fn handle(self: Arc<Self>, req: Request) -> Response {
        let host = srv::header_str(&req, &header::HOST);
        if let Some(file) = self.0.ca_file(req.uri().path()) {
            return file;
        }
        // At an address (a phone typing http://100.64.0.5), how to trust kiln first: HTTPS there
        // would only show a certificate warning.
        if req.uri().path() == "/" && is_address(host) && !self.0.ca_der.read().is_empty() {
            let r = srv::response(StatusCode::OK, "text/html; charset=utf-8", srv::full(self.0.trust_page()));
            return srv::with_header(r, header::CACHE_CONTROL, "no-cache");
        }
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

/// Plain HTTP on `port`: kiln's CA and, at an address, how to trust it; everything else
/// redirected to the HTTPS front door.
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
    fn plain_http_hands_out_the_ca_and_says_how_to_trust_it() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let pki = crate::pki::Pki::open(&dir.path().join("pki"))?;
            let r = Arc::new(Redirect::default());
            *r.names.write() = vec!["kiln.test".into(), "*.kiln.test".into()];
            *r.primary.write() = "kiln.test".into();
            r.https_port.store(443, Ordering::Relaxed);
            *r.ca_der.write() = pki.ca_der.clone();
            let port = crate::frontdoor::test_port()?;
            let _l = listen_redirect(port, &r)?;
            let client = crate::http::client::Client::public()?;
            // By address (127.0.0.1) or by a name (localhost, not one of the front door's).
            let get = |host: &str, path: &str| client.get(&format!("http://{host}:{port}{path}")).send();
            // The CA, three ways, over plain HTTP.
            let crt = get("127.0.0.1", "/ca.crt").await?;
            assert_eq!((crt.status(), crt.header("content-type").map(str::to_owned)), (200, Some("application/x-x509-ca-cert".to_owned())));
            assert_eq!(crt.bytes(1 << 16).await?.to_vec(), pki.ca_der);
            let mc = get("localhost", "/ca.mobileconfig").await?;
            assert_eq!(mc.header("content-type"), Some("application/x-apple-aspen-config"));
            assert!(String::from_utf8_lossy(&mc.bytes(1 << 16).await?).contains("com.apple.security.root"));
            let pem = String::from_utf8_lossy(&get("127.0.0.1", "/ca.pem").await?.bytes(1 << 16).await?).into_owned();
            assert!(pem.starts_with("-----BEGIN CERTIFICATE-----\n") && pem.trim_end().ends_with("-----END CERTIFICATE-----"));
            assert_eq!(crate::pki::cert_info(&pem).map(|i| i.sha256)?, crate::util::sha256_hex(&pki.ca_der));
            // At an address: the page that says how; at a name: the redirect.
            let page = get("127.0.0.1", "/").await?;
            assert_eq!(page.status(), 200);
            let html = String::from_utf8_lossy(&page.bytes(1 << 16).await?).into_owned();
            assert!(html.contains("/ca.mobileconfig") && html.contains("https://kiln.test/") && html.contains("Certificate Trust Settings"), "{html}");
            let named = get("localhost", "/").await?;
            assert_eq!((named.status(), named.header("location").map(str::to_owned)), (307, Some("https://kiln.test/".to_owned())));
            assert_eq!(get("127.0.0.1", "/runs").await?.status(), 307);
            // Without a CA to hand out, an address is redirected like any name.
            r.ca_der.write().clear();
            assert_eq!(get("127.0.0.1", "/").await?.status(), 307);
            assert_eq!(get("127.0.0.1", "/ca.crt").await?.status(), 307);
            Ok(())
        })
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

    /// Says which protocol carried the request.
    struct Said;

    impl srv::Handler for Said {
        async fn handle(self: Arc<Self>, req: Request) -> Response {
            srv::text(StatusCode::OK, &format!("{:?} {}", req.version(), srv::header_str(&req, &header::HOST).unwrap_or("-")))
        }
    }

    /// A kiln-like upstream on a free port: speaking HTTP/2 too (kiln's own listener) or only HTTP/1.1.
    fn upstream(h2: bool) -> anyhow::Result<SocketAddr> {
        let tcp = hyper_rt::tcp::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), 16).map_err(|e| anyhow::anyhow!("{e}"))?;
        let addr = tcp.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
        crate::rt::spawn(async move {
            while let Ok(stream) = tcp.accept().await {
                let remote = stream.peer_addr().ok();
                if h2 {
                    crate::rt::spawn(srv::serve_accepted(stream, Arc::new(Said), remote, None));
                } else {
                    crate::rt::spawn(srv::serve_connection(crate::http::io::RtIo::new(stream), Arc::new(Said), remote));
                }
            }
        });
        Ok(addr)
    }

    #[test]
    fn the_upstream_is_asked_whether_it_speaks_http2() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            assert!(speaks_h2(&Upstream::parse(&format!("http://{}", upstream(true)?), &[])?).await);
            assert!(!speaks_h2(&Upstream::parse(&format!("http://{}", upstream(false)?), &[])?).await);
            let closed = SocketAddr::from((Ipv4Addr::LOCALHOST, crate::frontdoor::test_port()?));
            assert!(!speaks_h2(&Upstream::parse(&format!("http://{closed}"), &[])?).await);
            Ok(())
        })
    }

    /// A browser's HTTP/2 goes through the front door to an upstream that speaks it; to one that
    /// doesn't, the front door offers only HTTP/1.1, and that goes through.
    #[test]
    fn http2_passes_through_when_the_upstream_speaks_it() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let pki = crate::pki::Pki::open(dir.path())?;
            let key = crate::pki::new_key_pem()?;
            let csr = crate::pki::front_door_csr(&["kiln.test".to_owned()], &key)?;
            let cert = pki.sign_front_door(&csr, &["kiln.test".to_owned()], 30)?;
            let roots = ClientTls::new(crate::http::client::Roots::Pem(pki.ca_pem.clone().into_bytes()), None)?;
            for h2_upstream in [true, false] {
                let shared = Arc::new(Shared::default());
                *shared.tls.write() = Some(ServerTls::from_pem(&cert, &key)?);
                *shared.upstream.write() = Some(Upstream::parse(&format!("http://{}", upstream(h2_upstream)?), &[])?);
                let port = crate::frontdoor::test_port()?;
                let _l = listen_https(port, &shared)?;
                // Asked at once.
                let until = Instant::now() + Duration::from_secs(5);
                while shared.h2.load(Ordering::Relaxed) != h2_upstream && Instant::now() < until {
                    crate::rt::sleep(Duration::from_millis(20)).await;
                }
                assert_eq!(shared.h2.load(Ordering::Relaxed), h2_upstream);
                // A browser asks for HTTP/2 first.
                let tcp = hyper_rt::tcp::TcpStream::connect(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).await.map_err(|e| anyhow::anyhow!("{e}"))?;
                let tls = TlsStream::connect(RtFuturesIo::new(tcp), roots.config_alpn(&[H2, HTTP11])?, crate::http::tls::server_name("kiln.test")?).await?;
                let body = |r: http::Response<kiln_http::Incoming>| async move {
                    use http_body_util::BodyExt as _;
                    Ok::<_, anyhow::Error>(String::from_utf8_lossy(&r.into_body().collect().await?.to_bytes()).into_owned())
                };
                let req = || http::Request::builder().uri("https://kiln.test/").header(header::HOST, "kiln.test").body(http_body_util::Empty::<bytes::Bytes>::new());
                if h2_upstream {
                    assert_eq!(tls.alpn_protocol(), Some(H2));
                    let (mut send, conn) = kiln_http::client::http2::handshake::<_, http_body_util::Empty<bytes::Bytes>>(tls).await?;
                    crate::rt::spawn(conn);
                    assert_eq!(body(send.send_request(req()?).await?).await?, "HTTP/2.0 kiln.test");
                    // Many at once, over the one connection.
                    let many = futures::future::join_all((0..8).map(|_| send.send_request(req().unwrap_or_default()))).await;
                    assert!(many.iter().all(|r| r.as_ref().is_ok_and(|r| r.status() == StatusCode::OK)));
                } else {
                    assert_eq!(tls.alpn_protocol(), Some(HTTP11));
                    let (mut send, conn) = kiln_http::client::http1::handshake::<_, http_body_util::Empty<bytes::Bytes>>(tls).await?;
                    crate::rt::spawn(conn);
                    assert_eq!(body(send.send_request(req()?).await?).await?, "HTTP/1.1 kiln.test");
                }
            }
            Ok(())
        })
    }
}
