//! Egress control (docs/ci.md W18-R08): a leg with a network policy, and every untrusted leg,
//! reaches only kiln's server and the hosts its policy allows.
//!
//! - **The proxy** runs in the runner, on the host: HTTP `CONNECT` tunnels, the TLS server name a
//!   client asks for checked against the policy too, and plain HTTP by its URL. A name that
//!   resolves to the host's own or a link-local address (a cloud's metadata service) is refused
//!   unless the policy names the address. What it refuses is annotated on the step.
//! - **Containers** sit on a network of the leg's own that leads nowhere (`--internal`). Their one
//!   way out is `kiln-egress:3128`, a relay in a container of its own: a static kiln (it runs in
//!   any image) that hands each connection to one the runner opened to it, through a port
//!   published on the host's loopback. Nothing in the leg ever connects to the host, whose
//!   firewall need not let containers in.
//! - **On the host executor** the proxy listens on loopback and steps get `HTTP(S)_PROXY`. It
//!   can't stop a step that ignores them, so untrusted legs never run there unless
//!   `ci.untrusted.executors` lists the host.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hyper_rt::tcp::TcpStream;

use crate::rt::Shutdown;

/// The relay's port for the leg's containers, and for the runner's connections.
pub const CLIENT_PORT: u16 = 3128;
pub const RUNNER_PORT: u16 = 3129;
/// What the leg's containers call the relay.
pub const ALIAS: &str = "kiln-egress";
/// Connections the runner keeps waiting at the relay.
const IDLE: usize = 8;
const HEAD_MAX: usize = 16 * 1024;

/// A static kiln for the relay's container: this one when it's built for musl (kiln's Linux
/// releases are), else `KILN_EGRESS_BINARY`.
pub fn relay_binary() -> Option<PathBuf> {
    if cfg!(all(target_os = "linux", target_env = "musl")) {
        return std::env::current_exe().ok();
    }
    std::env::var_os("KILN_EGRESS_BINARY").map(PathBuf::from).filter(|p| p.is_file())
}

/// What a leg may reach.
#[derive(Debug, Clone, Default)]
pub struct Policy {
    /// `kiln_ci::trust` patterns.
    pub allow: Vec<String>,
    /// kiln's server, `(host, port)`: always reachable.
    pub kiln: Vec<(String, u16)>,
}

impl Policy {
    /// The policy of `allow` for a runner whose server is at `urls`: their hosts are kiln's.
    pub fn new(allow: Vec<String>, urls: &[&str]) -> Policy {
        let mut kiln: Vec<(String, u16)> = Vec::new();
        for u in urls {
            let Some((scheme, rest)) = u.split_once("://") else { continue };
            let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
            let (host, port) = kiln_ci::trust::split_host(authority);
            let port = port.unwrap_or(if scheme.eq_ignore_ascii_case("https") { 443 } else { 80 });
            // The host's loopback, whatever the URL calls it: containers reach it as the host.
            let names: Vec<String> = if matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1") { vec!["127.0.0.1".into(), "localhost".into(), "::1".into()] } else { vec![host] };
            for h in names {
                if !host_empty(&h) && !kiln.contains(&(h.clone(), port)) {
                    kiln.push((h, port));
                }
            }
        }
        Policy { allow, kiln }
    }

    /// A leg's: `allow`, and kiln's server as the leg's runtime services reach it.
    pub fn for_leg(rt: &kiln_ci::wire::Runtime, allow: Vec<String>) -> Policy {
        let urls: Vec<&str> =
            [rt.results_url.as_str(), rt.cache_url.as_str(), rt.facade_url.as_str(), rt.api_url.as_str(), rt.graphql_url.as_str()].into_iter().chain(rt.id_token_url.as_deref()).collect();
        Policy::new(allow, &urls)
    }

    fn is_kiln(&self, host: &str, port: u16) -> bool {
        self.kiln.iter().any(|(h, p)| h.eq_ignore_ascii_case(host) && *p == port)
    }

    pub fn permits(&self, host: &str, port: u16) -> bool {
        self.is_kiln(host, port) || kiln_ci::trust::allowed(&self.allow, host, port)
    }

    /// Whether the policy names the address itself, so a local one may be reached.
    fn names(&self, ip: IpAddr, port: u16) -> bool {
        let s = ip.to_string();
        self.is_kiln(&s, port) || self.allow.iter().any(|a| matches!(kiln_ci::trust::split_host(a), (h, p) if h == s && p.is_none_or(|p| p == port)))
    }
}

fn host_empty(h: &str) -> bool {
    h.trim().is_empty()
}

/// What the containers call the host (`host.docker.internal`) is the runner's own loopback.
fn toward(host: &str) -> String {
    if host.eq_ignore_ascii_case(super::docker::HOST_ALIAS) { "127.0.0.1".to_owned() } else { host.to_ascii_lowercase() }
}

/// The host itself, or link-local (a cloud's metadata service at 169.254.169.254).
fn local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v.is_loopback() || v.is_link_local() || v.is_unspecified() || v.is_broadcast(),
        IpAddr::V6(v) => v.is_loopback() || v.is_unspecified() || v.segments().first().is_some_and(|s| s & 0xffc0 == 0xfe80) || v.to_ipv4_mapped().is_some_and(|m| m.is_loopback() || m.is_link_local()),
    }
}

/// Connects to `host:port` for a leg: a name of kiln's anywhere it resolves; another name not to
/// a local address unless the policy names that address.
async fn dial(policy: &Policy, host: &str, port: u16) -> Result<TcpStream, String> {
    let kiln = policy.is_kiln(host, port);
    let addrs = crate::rt::resolve_host(host, port).await.map_err(|e| format!("{e:#}"))?;
    let mut last = format!("{host}: no address");
    for a in addrs {
        if !kiln && local(a.ip()) && !policy.names(a.ip(), port) {
            last = format!("{host} is {}, an address of the host's own", a.ip());
            continue;
        }
        match crate::rt::timeout(Duration::from_secs(10), TcpStream::connect(a)).await {
            Ok(Ok(s)) => return Ok(s),
            Ok(Err(e)) => last = format!("connect {a}: {}", crate::rt::to_io(e)),
            Err(_) => last = format!("connect {a}: timed out"),
        }
    }
    Err(last)
}

async fn read_some(s: &TcpStream, buf: &mut [u8], within: Duration) -> Result<usize, String> {
    match crate::rt::timeout(within, s.read(buf)).await {
        Ok(Ok(n)) => Ok(n),
        Ok(Err(e)) => Err(crate::rt::to_io(e).to_string()),
        Err(_) => Err("timed out".into()),
    }
}

async fn write(s: &TcpStream, data: &[u8]) -> Result<(), String> {
    if data.is_empty() {
        return Ok(());
    }
    s.write_all(data).await.map_err(|e| crate::rt::to_io(e).to_string())
}

/// The request's head, to the blank line, and what came after it.
async fn read_head(s: &TcpStream) -> Result<(Vec<u8>, Vec<u8>), String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let rest = buf.split_off(end + 4);
            return Ok((buf, rest));
        }
        if buf.len() > HEAD_MAX {
            return Err("a request head over 16 KiB".into());
        }
        let n = read_some(s, &mut chunk, Duration::from_secs(60)).await?;
        if n == 0 {
            return Err("closed before a request".into());
        }
        buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
    }
}

async fn reply(s: &TcpStream, status: &str, body: &str) {
    let msg = format!("HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}\n", body.len() + 1);
    let _ = write(s, msg.as_bytes()).await;
}

/// Refuses a connection the policy doesn't allow, and reports it.
async fn refuse(s: &TcpStream, blocked: &async_channel::Sender<String>, what: String) {
    reply(s, "403 Forbidden", &format!("kiln's egress policy doesn't allow {what}")).await;
    let _ = blocked.try_send(what);
}

/// A byte reader over a TLS record, every read checked.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (a, b) = (self.0.get(..n)?, self.0.get(n..)?);
        self.0 = b;
        Some(a)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1)?.first().copied()
    }

    fn u16(&mut self) -> Option<usize> {
        let b = self.take(2)?;
        Some(usize::from(*b.first()?) << 8 | usize::from(*b.get(1)?))
    }

    fn u24(&mut self) -> Option<usize> {
        let b = self.take(3)?;
        Some(usize::from(*b.first()?) << 16 | usize::from(*b.get(1)?) << 8 | usize::from(*b.get(2)?))
    }
}

/// The length of the TLS record `rec` starts with, its header included.
fn record_len(rec: &[u8]) -> Option<usize> {
    let mut r = Reader(rec);
    if r.u8()? != 0x16 {
        return None;
    }
    r.take(2)?;
    Some(r.u16()? + 5)
}

/// The server name (SNI) a TLS ClientHello asks for, when `rec` holds the record whole.
pub fn sni(rec: &[u8]) -> Option<String> {
    let mut r = Reader(rec);
    if r.u8()? != 0x16 {
        return None;
    }
    r.take(2)?;
    let len = r.u16()?;
    let mut h = Reader(r.take(len)?);
    if h.u8()? != 0x01 {
        return None;
    }
    let hl = h.u24()?;
    let mut b = Reader(h.take(hl.min(h.0.len()))?);
    // Version, random, session id, cipher suites, compression methods.
    b.take(2 + 32)?;
    let sid = usize::from(b.u8()?);
    b.take(sid)?;
    let suites = b.u16()?;
    b.take(suites)?;
    let methods = usize::from(b.u8()?);
    b.take(methods)?;
    let ext_len = b.u16()?;
    let mut exts = Reader(b.take(ext_len)?);
    while !exts.0.is_empty() {
        let (kind, n) = (exts.u16()?, exts.u16()?);
        let mut data = Reader(exts.take(n)?);
        if kind != 0 {
            continue;
        }
        let list = data.u16()?;
        let mut names = Reader(data.take(list)?);
        while !names.0.is_empty() {
            let (ty, nl) = (names.u8()?, names.u16()?);
            let name = names.take(nl)?;
            if ty == 0 {
                return std::str::from_utf8(name).ok().map(|s| s.to_ascii_lowercase());
            }
        }
    }
    None
}

/// One connection from a leg: a `CONNECT` tunnel or a plain HTTP request, if the policy lets it
/// through; else refused, and reported on `blocked`.
pub async fn serve(client: TcpStream, policy: Arc<Policy>, blocked: async_channel::Sender<String>) {
    if let Err(e) = serve_one(Arc::new(client), &policy, &blocked).await {
        tracing::debug!(error = %e, "an egress connection ended");
    }
}

async fn serve_one(conn: Arc<TcpStream>, policy: &Policy, blocked: &async_channel::Sender<String>) -> Result<(), String> {
    let client = conn.as_ref();
    let (head, mut rest) = read_head(client).await?;
    let text = String::from_utf8_lossy(&head).into_owned();
    let mut words = text.lines().next().unwrap_or_default().split_whitespace();
    let (method, target, version) = (words.next().unwrap_or_default().to_owned(), words.next().unwrap_or_default().to_owned(), words.next().unwrap_or("HTTP/1.1").to_owned());
    if method.eq_ignore_ascii_case("CONNECT") {
        let (named, port) = kiln_ci::trust::split_host(&target);
        let port = port.unwrap_or(443);
        let host = toward(&named);
        if !policy.permits(&host, port) {
            refuse(client, blocked, format!("{named}:{port}")).await;
            return Ok(());
        }
        let up = match dial(policy, &host, port).await {
            Ok(u) => u,
            Err(e) => {
                reply(client, "502 Bad Gateway", &e).await;
                return Err(e);
            }
        };
        write(client, b"HTTP/1.1 200 Connection Established\r\n\r\n").await?;
        // A TLS client's first record names the server it wants: that must be allowed too, so
        // an allowed host can't front for another.
        let mut chunk = [0u8; 4096];
        if rest.is_empty() {
            let n = read_some(client, &mut chunk, Duration::from_secs(10)).await.unwrap_or(0);
            rest.extend_from_slice(chunk.get(..n).unwrap_or_default());
        }
        while rest.first() == Some(&0x16) && rest.len() < record_len(&rest).unwrap_or(0).min(HEAD_MAX) {
            let n = read_some(client, &mut chunk, Duration::from_secs(10)).await?;
            if n == 0 {
                break;
            }
            rest.extend_from_slice(chunk.get(..n).unwrap_or_default());
        }
        if let Some(name) = sni(&rest)
            && name != host
            && name != named.to_ascii_lowercase()
            && !policy.permits(&name, port)
        {
            let _ = blocked.try_send(format!("{name}:{port}"));
            return Err(format!("the tunnel to {named}:{port} asked TLS for {name}"));
        }
        write(&up, &rest).await?;
        relay(conn, up).await;
        return Ok(());
    }
    // Plain HTTP through the proxy: an absolute URL.
    let Some(url) = target.strip_prefix("http://") else {
        reply(client, "400 Bad Request", "kiln's egress proxy takes CONNECT and http:// requests").await;
        return Ok(());
    };
    let (authority, path) = url.split_once('/').map_or((url, "/".to_owned()), |(a, p)| (a, format!("/{p}")));
    let (named, port) = kiln_ci::trust::split_host(authority);
    let port = port.unwrap_or(80);
    let host = toward(&named);
    if !policy.permits(&host, port) {
        refuse(client, blocked, format!("{named}:{port}")).await;
        return Ok(());
    }
    let up = match dial(policy, &host, port).await {
        Ok(u) => u,
        Err(e) => {
            reply(client, "502 Bad Gateway", &e).await;
            return Err(e);
        }
    };
    // The request as the server takes it: origin-form, without the proxy's headers.
    let mut out = format!("{method} {path} {version}\r\n");
    for h in text.split("\r\n").skip(1).filter(|h| !h.is_empty()) {
        let name = h.split(':').next().unwrap_or_default().trim();
        if !name.eq_ignore_ascii_case("proxy-connection") && !name.eq_ignore_ascii_case("proxy-authorization") {
            out.push_str(h);
            out.push_str("\r\n");
        }
    }
    out.push_str("\r\n");
    write(&up, out.as_bytes()).await?;
    write(&up, &rest).await?;
    relay(conn, up).await;
    Ok(())
}

/// Copies both ways until each side is done: the server's answers on a task of their own.
async fn relay(client: Arc<TcpStream>, up: TcpStream) {
    let up = Arc::new(up);
    let (from, to) = (Arc::clone(&up), Arc::clone(&client));
    let back = crate::rt::spawn(async move { pipe(&from, &to).await });
    pipe(&client, &up).await;
    let _ = back.await;
}

async fn pipe(from: &TcpStream, to: &TcpStream) {
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        match from.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if to.write_all(buf.get(..n).unwrap_or_default()).await.is_err() {
                    break;
                }
            }
        }
    }
    let _ = to.shutdown(hyper_rt::tcp::Shutdown::Write);
}

/// The runner's side of a leg's relay: connections waiting at the relay's published port, each
/// serving one of the leg's connections when the relay hands it one.
pub struct Pool {
    stop: Shutdown,
    blocked: async_channel::Receiver<String>,
}

impl Pool {
    pub fn start(port: u16, token: String, policy: Policy) -> Pool {
        let (tx, blocked) = async_channel::unbounded();
        let stop = Shutdown::new();
        let (token, policy) = (Arc::new(token), Arc::new(policy));
        for _ in 0..IDLE {
            crate::rt::spawn(wait_at_relay(port, Arc::clone(&token), Arc::clone(&policy), tx.clone(), stop.clone()));
        }
        Pool { stop, blocked }
    }

    /// The proxy on the host's loopback, for steps on the host: its port.
    pub async fn local(policy: Policy) -> anyhow::Result<(Pool, u16)> {
        let tcp = hyper_rt::tcp::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)), 64).map_err(|e| anyhow::anyhow!("{}", crate::rt::to_io(e)))?;
        let port = tcp.local_addr().map_err(|e| anyhow::anyhow!("{}", crate::rt::to_io(e)))?.port();
        let (tx, blocked) = async_channel::unbounded();
        let stop = Shutdown::new();
        let (policy, s) = (Arc::new(policy), stop.clone());
        crate::rt::spawn(async move {
            loop {
                let accepted = {
                    let a = std::pin::pin!(tcp.accept());
                    let c = std::pin::pin!(s.cancelled());
                    match futures::future::select(a, c).await {
                        futures::future::Either::Left((Ok(conn), _)) => Some(conn),
                        _ => None,
                    }
                };
                let Some(conn) = accepted else { return };
                crate::rt::spawn(serve(conn, Arc::clone(&policy), tx.clone()));
            }
        });
        Ok((Pool { stop, blocked }, port))
    }

    /// A proxy that runs elsewhere (a microVM's leg's, on its host: docs/ci.md W13-R05), whose
    /// refusals come on `blocked`.
    pub fn remote(blocked: async_channel::Receiver<String>) -> Pool {
        Pool { stop: Shutdown::new(), blocked }
    }

    /// What was refused since last asked.
    pub fn blocked(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        while let Ok(b) = self.blocked.try_recv() {
            if !out.contains(&b) {
                out.push(b);
            }
        }
        out
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

/// One connection waiting at the relay, again and again until the leg ends.
async fn wait_at_relay(port: u16, token: Arc<String>, policy: Arc<Policy>, blocked: async_channel::Sender<String>, stop: Shutdown) {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut backoff = Duration::from_millis(50);
    while !stop.is_cancelled() {
        let conn = match crate::rt::timeout(Duration::from_secs(5), TcpStream::connect(addr)).await {
            Ok(Ok(c)) => c,
            _ => {
                if !stop.sleep(backoff).await {
                    return;
                }
                backoff = (backoff * 2).min(Duration::from_secs(2));
                continue;
            }
        };
        backoff = Duration::from_millis(50);
        if write(&conn, format!("KILN-EGRESS {token}\n").as_bytes()).await.is_err() {
            continue;
        }
        // Until the relay hands it a connection of the leg's, or the leg ends.
        let mut go = [0u8; 3];
        let got = {
            let r = std::pin::pin!(read_exact(&conn, &mut go));
            let c = std::pin::pin!(stop.cancelled());
            matches!(futures::future::select(r, c).await, futures::future::Either::Left((true, _)))
        };
        if !got {
            continue;
        }
        if &go == b"GO\n" {
            crate::rt::spawn(serve(conn, Arc::clone(&policy), blocked.clone()));
        }
    }
}

async fn read_exact(s: &TcpStream, buf: &mut [u8]) -> bool {
    let mut at = 0;
    while at < buf.len() {
        let Some(rest) = buf.get_mut(at..) else { return false };
        match s.read(rest).await {
            Ok(0) | Err(_) => return false,
            Ok(n) => at += n,
        }
    }
    true
}

/// `kiln egress-relay` (in a leg's relay container): each connection of the leg's on
/// [`CLIENT_PORT`] goes to one the runner opened on [`RUNNER_PORT`] (`KILN_EGRESS_<token>`).
pub fn relay_main() -> anyhow::Result<()> {
    use std::io::{Read as _, Write as _};
    let token = std::env::var("KILN_EGRESS_TOKEN").map_err(|_| anyhow::anyhow!("KILN_EGRESS_TOKEN isn't set"))?;
    let clients = std::net::TcpListener::bind(("0.0.0.0", CLIENT_PORT))?;
    let runners = std::net::TcpListener::bind(("0.0.0.0", RUNNER_PORT))?;
    let (tx, rx) = std::sync::mpsc::channel::<std::net::TcpStream>();
    let rx = Arc::new(std::sync::Mutex::new(rx));
    std::thread::spawn(move || {
        for conn in runners.incoming().flatten() {
            let (tx, want) = (tx.clone(), format!("KILN-EGRESS {token}\n"));
            std::thread::spawn(move || {
                let mut conn = conn;
                let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
                let mut got = vec![0u8; want.len()];
                if conn.read_exact(&mut got).is_ok() && crate::util::ct_eq(&got, want.as_bytes()) {
                    let _ = conn.set_read_timeout(None);
                    let _ = tx.send(conn);
                }
            });
        }
    });
    for client in clients.incoming().flatten() {
        let rx = Arc::clone(&rx);
        std::thread::spawn(move || {
            let until = Instant::now() + Duration::from_secs(30);
            while let Some(left) = until.checked_duration_since(Instant::now()) {
                let next = match rx.lock() {
                    Ok(r) => r.recv_timeout(left),
                    Err(_) => return,
                };
                let Ok(mut runner) = next else { return };
                if runner.write_all(b"GO\n").is_ok() {
                    splice(client, runner);
                    return;
                }
            }
        });
    }
    Ok(())
}

/// Both ways between two sockets, each closed for writing when its source ends.
fn splice(a: std::net::TcpStream, b: std::net::TcpStream) {
    let (Ok(mut a2), Ok(mut b2)) = (a.try_clone(), b.try_clone()) else { return };
    let t = std::thread::spawn(move || {
        let _ = std::io::copy(&mut a2, &mut b2);
        let _ = b2.shutdown(std::net::Shutdown::Write);
    });
    let (mut a, mut b) = (a, b);
    let _ = std::io::copy(&mut b, &mut a);
    let _ = a.shutdown(std::net::Shutdown::Write);
    let _ = t.join();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ClientHello naming `name`, as a TLS 1.3 client sends it.
    fn hello(name: &str) -> Vec<u8> {
        let n = name.as_bytes();
        let mut sni = vec![0u8, 0];
        let entry_len = 3 + n.len();
        sni.extend_from_slice(&(u16::try_from(entry_len + 2).unwrap_or(0)).to_be_bytes());
        sni.extend_from_slice(&(u16::try_from(entry_len).unwrap_or(0)).to_be_bytes());
        sni.push(0);
        sni.extend_from_slice(&(u16::try_from(n.len()).unwrap_or(0)).to_be_bytes());
        sni.extend_from_slice(n);
        let other = [0x00u8, 0x2b, 0x00, 0x03, 0x02, 0x03, 0x04];
        let mut body = vec![0x03, 0x03];
        body.extend_from_slice(&[7u8; 32]);
        body.push(0);
        body.extend_from_slice(&[0x00, 0x02, 0x13, 0x01, 0x01, 0x00]);
        let exts: Vec<u8> = other.iter().chain(sni.iter()).copied().collect();
        body.extend_from_slice(&(u16::try_from(exts.len()).unwrap_or(0)).to_be_bytes());
        body.extend_from_slice(&exts);
        let mut hs = vec![0x01];
        hs.extend_from_slice(u32::try_from(body.len()).unwrap_or(0).to_be_bytes().get(1..).unwrap_or_default());
        hs.extend_from_slice(&body);
        let mut rec = vec![0x16, 0x03, 0x01];
        rec.extend_from_slice(&(u16::try_from(hs.len()).unwrap_or(0)).to_be_bytes());
        rec.extend_from_slice(&hs);
        rec
    }

    #[test]
    fn w18_r08_sni() {
        let rec = hello("Registry.NPMJS.org");
        assert_eq!(record_len(&rec), Some(rec.len()));
        assert_eq!(sni(&rec).as_deref(), Some("registry.npmjs.org"));
        assert_eq!(sni(rec.get(..rec.len() - 4).unwrap_or_default()), None, "a part of it");
        assert_eq!(sni(b"GET / HTTP/1.1\r\n"), None);
    }

    #[test]
    fn w18_r08_policy() {
        let p = Policy::new(vec!["*.pypi.org".into()], &["http://127.0.0.1:17070/", "https://kiln.example/api/v3"]);
        assert!(p.permits("127.0.0.1", 17070) && p.permits("localhost", 17070) && p.permits("kiln.example", 443));
        assert!(p.permits("files.pypi.org", 443) && !p.permits("evil.example", 443) && !p.permits("127.0.0.1", 22));
        assert_eq!(toward("host.docker.internal"), "127.0.0.1");
        assert!(local("169.254.169.254".parse().unwrap_or(IpAddr::from([0, 0, 0, 0]))) && local("::1".parse().unwrap_or(IpAddr::from([0, 0, 0, 0]))) && !local("10.0.0.5".parse().unwrap_or(IpAddr::from([127, 0, 0, 1]))));
        assert!(p.names("127.0.0.1".parse().unwrap_or(IpAddr::from([0, 0, 0, 0])), 17070));
    }

    /// A leg's proxy over the loopback: allowed hosts through, others refused and reported.
    #[test]
    fn w18_r08_proxy() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            // An "allowed" server, and one the policy doesn't name, both on loopback.
            let up = hyper_rt::tcp::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)), 8).map_err(|e| anyhow::anyhow!("{}", crate::rt::to_io(e)))?;
            let up_port = up.local_addr().map_err(|e| anyhow::anyhow!("{}", crate::rt::to_io(e)))?.port();
            crate::rt::spawn(async move {
                while let Ok(s) = up.accept().await {
                    crate::rt::spawn(async move {
                        let mut b = [0u8; 1024];
                        let n = s.read(&mut b).await.unwrap_or(0);
                        let got = String::from_utf8_lossy(b.get(..n).unwrap_or_default()).into_owned();
                        let body = if got.starts_with("GET /hello HTTP/1.1") && !got.to_ascii_lowercase().contains("proxy-connection") { "hi" } else { "bad" };
                        let _ = s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{body}").as_bytes()).await;
                    });
                }
            });
            let policy = Policy::new(vec![format!("127.0.0.1:{up_port}")], &[]);
            let (pool, port) = Pool::local(policy).await?;
            let ask = |req: String| async move {
                let s = TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], port))).await.map_err(|e| anyhow::anyhow!("{}", crate::rt::to_io(e)))?;
                s.write_all(req.as_bytes()).await.map_err(|e| anyhow::anyhow!("{}", crate::rt::to_io(e)))?;
                let mut out = Vec::new();
                let mut b = [0u8; 1024];
                loop {
                    let n = s.read(&mut b).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    out.extend_from_slice(b.get(..n).unwrap_or_default());
                }
                anyhow::Ok(String::from_utf8_lossy(&out).into_owned())
            };
            let ok = ask(format!("GET http://127.0.0.1:{up_port}/hello HTTP/1.1\r\nHost: 127.0.0.1:{up_port}\r\nProxy-Connection: keep-alive\r\n\r\n")).await?;
            assert!(ok.starts_with("HTTP/1.1 200") && ok.ends_with("hi"), "{ok}");
            let no = ask("CONNECT evil.example:443 HTTP/1.1\r\nHost: evil.example:443\r\n\r\n".into()).await?;
            assert!(no.starts_with("HTTP/1.1 403"), "{no}");
            // The tunnel to an allowed host carries what the client sends.
            let tunnel = ask(format!("CONNECT 127.0.0.1:{up_port} HTTP/1.1\r\n\r\nGET /hello HTTP/1.1\r\nHost: x\r\n\r\n")).await?;
            assert!(tunnel.contains("200 Connection Established") && tunnel.ends_with("hi"), "{tunnel}");
            assert_eq!(pool.blocked(), vec!["evil.example:443".to_owned()]);
            anyhow::Ok(())
        })
    }
}
