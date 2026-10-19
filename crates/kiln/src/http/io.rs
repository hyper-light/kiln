//! hyper-rt TCP streams as hyper IO (`hyper::rt::{Read, Write}`) and futures-io, hyper's timer on
//! the shard's wheel, and the one-request HTTP(S) client connection.
//!
//! hyper-rt sockets are readiness-based: `try_read` / `try_write` never wait, and `readable()` /
//! `writable()` give a one-shot wait that must be kept between polls (dropping it withdraws the
//! registration). That wait only accepts the polling task's own waker; under a foreign waker
//! (async-tungstenite's proxy, for one) the wait runs in a [`Bridge`] helper task instead.

use std::borrow::Borrow;
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use hyper::body::Incoming;
use hyper_rt::RtError;
use hyper_rt::readiness::Ready;
use hyper_rt::tcp::{Shutdown, TcpStream};

use super::server::Body;
use super::tls::{ClientTls, TlsStream};
use crate::rt::{Bridge, is_native, to_io};

/// One direction's outstanding wait.
enum Wait {
    /// Registered for the polling task itself (the fast path).
    Native(Ready),
    /// Registered by a helper task that wakes a foreign waker.
    Bridged(Bridge<Result<(), RtError>>),
}

/// Try the non-blocking call; when it would block, arm readiness (natively or through a bridge) and
/// return `Pending`; when readiness fires, try again.
fn poll_io<T>(
    wait: &mut Option<Wait>,
    stream: &TcpStream,
    writable: bool,
    cx: &mut Context<'_>,
    mut attempt: impl FnMut(&TcpStream) -> Result<Option<T>, RtError>,
) -> Poll<io::Result<T>> {
    let native = is_native(cx);
    loop {
        match wait.take() {
            None => {}
            Some(Wait::Native(mut ready)) => {
                if native {
                    match Pin::new(&mut ready).poll(cx) {
                        Poll::Pending => {
                            *wait = Some(Wait::Native(ready));
                            return Poll::Pending;
                        }
                        Poll::Ready(outcome) => outcome.map_err(to_io)?,
                    }
                }
                // A native wait now polled under a foreign waker is withdrawn and re-armed below.
            }
            Some(Wait::Bridged(bridge)) => match bridge.poll(cx) {
                Poll::Pending => {
                    *wait = Some(Wait::Bridged(bridge));
                    return Poll::Pending;
                }
                Poll::Ready(outcome) => outcome.map_err(to_io)?,
            },
        }
        match attempt(stream) {
            Ok(Some(value)) => return Poll::Ready(Ok(value)),
            Err(error) => return Poll::Ready(Err(to_io(error))),
            Ok(None) => {
                let ready = if writable { stream.writable() } else { stream.readable() };
                if native {
                    *wait = Some(Wait::Native(ready));
                } else {
                    *wait = Some(Wait::Bridged(Bridge::spawn(ready, cx.waker()).map_err(to_io)?));
                    return Poll::Pending;
                }
            }
        }
    }
}

/// A stream plus one kept wait per direction.
struct Halves<S: Borrow<TcpStream>> {
    // Waits drop (withdraw) before the stream.
    read_wait: Option<Wait>,
    write_wait: Option<Wait>,
    stream: S,
}

impl<S: Borrow<TcpStream>> Halves<S> {
    fn new(stream: S) -> Self {
        Self { read_wait: None, write_wait: None, stream }
    }

    fn poll_read_slice(&mut self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        poll_io(&mut self.read_wait, self.stream.borrow(), false, cx, |s| s.try_read(buf))
    }

    fn poll_write_slice(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        poll_io(&mut self.write_wait, self.stream.borrow(), true, cx, |s| s.try_write(buf))
    }

    fn shutdown_write(&self) -> io::Result<()> {
        self.stream.borrow().shutdown(Shutdown::Write).map_err(to_io)
    }
}

/// hyper's first read buffer is 8 KiB and grows; 16 KiB serves most reads in one syscall.
const SCRATCH: usize = 16 * 1024;

/// `hyper::rt::Read + Write` over an owned stream. Safe code: one copy per read through a scratch
/// buffer, because hyper's uninitialised read cursor is only reachable through `unsafe`.
pub struct RtIo {
    inner: Halves<TcpStream>,
    scratch: Box<[u8]>,
}

impl RtIo {
    pub fn new(stream: TcpStream) -> Self {
        Self { inner: Halves::new(stream), scratch: vec![0u8; SCRATCH].into_boxed_slice() }
    }

    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.inner.stream.peer_addr().ok()
    }
}

impl hyper::rt::Read for RtIo {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, mut buf: hyper::rt::ReadBufCursor<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let want = buf.remaining().min(this.scratch.len());
        if want == 0 {
            return Poll::Ready(Ok(()));
        }
        let Some(scratch) = this.scratch.get_mut(..want) else {
            return Poll::Ready(Ok(()));
        };
        match this.inner.poll_read_slice(cx, scratch) {
            Poll::Ready(Ok(n)) => {
                if let Some(read) = scratch.get(..n) {
                    buf.put_slice(read);
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl hyper::rt::Write for RtIo {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        self.get_mut().inner.poll_write_slice(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // No user-space buffering: every write went to the kernel.
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(self.get_mut().inner.shutdown_write())
    }
}

/// `futures::io::{AsyncRead, AsyncWrite}` over an owned stream (the TLS layer's transport).
pub struct RtFuturesIo {
    inner: Halves<TcpStream>,
}

impl RtFuturesIo {
    pub fn new(stream: TcpStream) -> Self {
        Self { inner: Halves::new(stream) }
    }
}

impl futures::io::AsyncRead for RtFuturesIo {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        self.get_mut().inner.poll_read_slice(cx, buf)
    }
}

impl futures::io::AsyncWrite for RtFuturesIo {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        self.get_mut().inner.poll_write_slice(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(self.get_mut().inner.shutdown_write())
    }
}

/// `hyper::rt::Timer` on the shard's timing wheel (header read timeouts, keep-alive).
#[derive(Clone, Copy, Debug, Default)]
pub struct RtTimer;

struct HyperSleep(crate::rt::Sleep);

impl Future for HyperSleep {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        Pin::new(&mut self.0).poll(cx)
    }
}

impl hyper::rt::Sleep for HyperSleep {}

impl hyper::rt::Timer for RtTimer {
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn hyper::rt::Sleep>> {
        Box::pin(HyperSleep(crate::rt::Sleep::until(Instant::now() + duration)))
    }

    fn sleep_until(&self, deadline: Instant) -> Pin<Box<dyn hyper::rt::Sleep>> {
        Box::pin(HyperSleep(crate::rt::Sleep::until(deadline)))
    }
}

/// `hyper::rt::Executor` for HTTP/2: each request of a connection runs as its own hyper-rt task
/// (the connection's own task does the reading and writing).
#[derive(Clone, Copy, Debug, Default)]
pub struct RtExec;

impl<F> hyper::rt::Executor<F> for RtExec
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    fn execute(&self, fut: F) {
        // Detached: it ends with its request.
        drop(crate::rt::spawn(fut));
    }
}

/// Time allowed for each address's TCP connect.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// A TCP connection to `host:port` (a name or an address), trying each address in turn.
pub(crate) async fn connect(host: &str, port: u16) -> anyhow::Result<TcpStream> {
    let mut addrs = crate::rt::resolve_host(host, port).await?;
    // IPv4 first: a dual-stack name often has an unreachable IPv6 address here.
    addrs.sort_by_key(|a| !a.is_ipv4());
    let mut last = None;
    for addr in addrs {
        match crate::rt::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(e)) => last = Some(format!("connect {addr}: {}", to_io(e))),
            Err(_) => last = Some(format!("connect {addr}: timed out")),
        }
    }
    Err(anyhow::anyhow!(last.unwrap_or_else(|| format!("{host}: no address"))))
}

/// Where a request to an address someone supplied may go (docs/ci.md W22-AC07): the public
/// internet, never this machine, its private networks or link-local addresses (a cloud's metadata
/// service) unless `allow` lets that host, address or network through. The name is resolved here
/// and the connection made to the very address checked, so a name that answers differently the
/// next time can't get around it.
#[derive(Debug, Clone, Default)]
pub struct Reach {
    /// Hosts (`hooks.corp`, `hooks.corp:8443`, `*.corp`), addresses and networks (`10.0.0.0/8`).
    pub allow: Vec<String>,
}

impl Reach {
    pub fn allows(&self, host: &str, ip: IpAddr, port: u16) -> bool {
        public(ip)
            || self.allow.iter().any(|a| match crate::discovery::parse_cidr(a) {
                Some((net, len)) => crate::discovery::in_net(ip, net, len),
                None => kiln_ci::trust::host_matches(a, host, port) || kiln_ci::trust::host_matches(a, &ip.to_string(), port),
            })
    }

    /// The addresses of `host` a request may go to, or why none.
    pub async fn addresses(&self, host: &str, port: u16) -> anyhow::Result<Vec<SocketAddr>> {
        let addrs = crate::rt::resolve_host(host, port).await?;
        let (ok, refused): (Vec<SocketAddr>, Vec<SocketAddr>) = addrs.into_iter().partition(|a| self.allows(host, a.ip(), port));
        match (ok.is_empty(), refused.first()) {
            (true, Some(a)) => anyhow::bail!("{host} is {}, a private address: kiln sends nothing there unless `[ci] allow_private_targets` lists it", a.ip()),
            (true, None) => anyhow::bail!("{host}: no address"),
            _ => Ok(ok),
        }
    }

    /// Whether `url`'s host is one a request may go to.
    pub async fn check_url(&self, url: &str) -> anyhow::Result<()> {
        let uri: http::Uri = url.parse().map_err(|e| anyhow::anyhow!("bad url {url}: {e}"))?;
        let host = uri.host().ok_or_else(|| anyhow::anyhow!("no host in {url}"))?;
        let port = uri.port_u16().unwrap_or(if uri.scheme_str() == Some("https") { 443 } else { 80 });
        self.addresses(host.trim_start_matches('[').trim_end_matches(']'), port).await.map(|_| ())
    }
}

/// Whether `ip` is an address of the public internet.
pub fn public(ip: IpAddr) -> bool {
    let v4_special = |v: std::net::Ipv4Addr| v.is_broadcast() || v.is_unspecified() || v.is_multicast() || v.octets().first() == Some(&0) || crate::discovery::is_private(IpAddr::V4(v));
    match ip {
        IpAddr::V4(v) => !v4_special(v),
        IpAddr::V6(v) => !(v.is_unspecified() || v.is_multicast() || crate::discovery::is_private(ip) || v.to_ipv4_mapped().is_some_and(v4_special)),
    }
}

/// A TCP connection to `host:port` for a request that `reach` limits: through this machine's
/// proxy when it has one (the name was checked here first), else to an address checked.
pub(crate) async fn connect_reaching(host: &str, port: u16, https: bool, reach: &Reach) -> anyhow::Result<TcpStream> {
    use anyhow::Context as _;
    if let Some(proxy) = super::proxy::settings().choose(https, host, port) {
        // Where it goes is checked all the same.
        reach.addresses(host, port).await?;
        let tcp = connect(&proxy.host, proxy.port).await.with_context(|| format!("the proxy {}:{} (for {host}:{port})", proxy.host, proxy.port))?;
        super::proxy::tunnel(&tcp, proxy, host, port).await?;
        return Ok(tcp);
    }
    connect_direct_reaching(host, port, reach).await
}

/// [`connect_reaching`] never through a proxy, for what an HTTP proxy doesn't carry (SMTP).
pub(crate) async fn connect_direct_reaching(host: &str, port: u16, reach: &Reach) -> anyhow::Result<TcpStream> {
    let mut addrs = reach.addresses(host, port).await?;
    addrs.sort_by_key(|a| !a.is_ipv4());
    let mut last = None;
    for addr in addrs {
        match crate::rt::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(e)) => last = Some(format!("connect {addr}: {}", to_io(e))),
            Err(_) => last = Some(format!("connect {addr}: timed out")),
        }
    }
    Err(anyhow::anyhow!(last.unwrap_or_else(|| format!("{host}: no address"))))
}

/// A TCP connection out to `host:port` for HTTP (TLS when `https`): through the proxy this
/// machine's network wants ([`super::proxy`]), else direct.
pub(crate) async fn connect_out(host: &str, port: u16, https: bool) -> anyhow::Result<TcpStream> {
    use anyhow::Context as _;
    let Some(proxy) = super::proxy::settings().choose(https, host, port) else { return connect(host, port).await };
    let tcp = connect(&proxy.host, proxy.port).await.with_context(|| format!("the proxy {}:{} (for {host}:{port})", proxy.host, proxy.port))?;
    super::proxy::tunnel(&tcp, proxy, host, port).await?;
    Ok(tcp)
}

/// One request on a fresh connection (HTTP/1.1; TLS when `https`). The connection is driven by a
/// task on this shard and closes once the response body has been read and the request dropped.
pub async fn send(
    tls: &ClientTls,
    host: &str,
    port: u16,
    https: bool,
    req: http::Request<Body>,
) -> anyhow::Result<http::Response<Incoming>> {
    send_reaching(tls, host, port, https, req, None).await
}

/// [`send`], to only where `reach` lets a request go when it limits it.
pub async fn send_reaching(tls: &ClientTls, host: &str, port: u16, https: bool, req: http::Request<Body>, reach: Option<&Reach>) -> anyhow::Result<http::Response<Incoming>> {
    let tcp = match reach {
        Some(r) => connect_reaching(host, port, https, r).await?,
        None => connect_out(host, port, https).await?,
    };
    let mut sender = if https {
        let stream = TlsStream::connect(RtFuturesIo::new(tcp), tls.config()?, super::tls::server_name(host)?)
            .await
            .map_err(|e| anyhow::anyhow!("TLS to {host}: {e}"))?;
        let (sender, conn) = hyper::client::conn::http1::handshake::<_, Body>(stream).await?;
        crate::rt::spawn_local(async move {
            let _ = conn.await;
        })
        .map_err(|e| anyhow::anyhow!("spawn connection task: {e}"))?;
        sender
    } else {
        let (sender, conn) = hyper::client::conn::http1::handshake::<_, Body>(RtIo::new(tcp)).await?;
        crate::rt::spawn_local(async move {
            let _ = conn.await;
        })
        .map_err(|e| anyhow::anyhow!("spawn connection task: {e}"))?;
        sender
    };
    Ok(sender.send_request(req).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap_or(IpAddr::from([0, 0, 0, 0]))
    }

    /// W22-AC07: a request to an address someone supplied never reaches this machine, a private
    /// network or a metadata service unless allowed, even through a name that points there.
    #[test]
    fn w22_ac07_reach() -> anyhow::Result<()> {
        for private in ["127.0.0.1", "10.1.2.3", "172.20.0.1", "192.168.1.5", "169.254.169.254", "100.100.1.1", "0.0.0.0", "255.255.255.255", "224.0.0.1", "::1", "fd00::1", "fe80::1", "::ffff:127.0.0.1", "::"] {
            assert!(!public(ip(private)), "{private} isn't public");
        }
        for public_ip in ["1.1.1.1", "140.82.112.3", "2606:4700:4700::1111"] {
            assert!(public(ip(public_ip)), "{public_ip} is public");
        }
        let none = Reach::default();
        assert!(!none.allows("hooks.corp", ip("10.0.0.7"), 443) && none.allows("example.com", ip("93.184.216.34"), 443));
        let some = Reach { allow: vec!["127.0.0.1:18080".into(), "10.0.0.0/8".into(), "*.corp".into()] };
        assert!(some.allows("127.0.0.1", ip("127.0.0.1"), 18080) && !some.allows("127.0.0.1", ip("127.0.0.1"), 22));
        assert!(some.allows("db", ip("10.9.9.9"), 5432) && some.allows("hooks.corp", ip("192.168.0.4"), 443) && !some.allows("evil", ip("192.168.0.4"), 443));
        crate::rt::block_on(async move {
            // A name that resolves to the host itself is refused, saying why.
            let e = Reach::default().check_url("http://localhost:9/hook").await.err().map(|e| format!("{e:#}")).unwrap_or_default();
            anyhow::ensure!(e.contains("private address") && e.contains("allow_private_targets"), "{e}");
            anyhow::ensure!(Reach { allow: vec!["localhost:9".into()] }.check_url("http://localhost:9/hook").await.is_ok());
            anyhow::Ok(())
        })
    }
}
