//! kiln's connections: hyper-rt TCP streams as `futures-io` (kiln-http's [`TcpIo`]), the TCP
//! connections out (direct, through this machine's proxy, or only to where a request may go), and the
//! one-request HTTP(S) client connection.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use hyper_rt::tcp::TcpStream;
pub use kiln_http::TcpIo;
use kiln_http::Incoming;

use super::server::Body;
use super::tls::{ClientTls, TlsStream};
use crate::rt::to_io;

/// A hyper-rt TCP stream as `futures-io`, what kiln's HTTP serves and speaks over.
pub type RtIo = TcpIo;

/// The same, by the name the TLS layer's callers know it by.
pub type RtFuturesIo = TcpIo;

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
        let stream = TlsStream::connect(TcpIo::new(tcp), tls.config()?, super::tls::server_name(host)?)
            .await
            .map_err(|e| anyhow::anyhow!("TLS to {host}: {e}"))?;
        let (sender, conn) = kiln_http::client::http1::handshake::<_, Body>(stream).await?;
        crate::rt::spawn_local(async move {
            let _ = conn.await;
        })
        .map_err(|e| anyhow::anyhow!("spawn connection task: {e}"))?;
        sender
    } else {
        let (sender, conn) = kiln_http::client::http1::handshake::<_, Body>(TcpIo::new(tcp)).await?;
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
