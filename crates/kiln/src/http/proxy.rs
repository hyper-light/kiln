//! Outbound HTTP(S) through a proxy, for machines whose network lets traffic out only through
//! one (a company proxy, Zscaler in its explicit-proxy mode). Taken from `HTTPS_PROXY`,
//! `HTTP_PROXY` and `ALL_PROXY` (or their lowercase forms), else, on macOS, the system's proxy
//! settings (not a PAC file). Every connection goes through a `CONNECT` tunnel, plain HTTP
//! included, so WebSockets (a runner's HTTP tunnel to the server) work through it too.
//!
//! What never goes through one: loopback and private-network addresses and `localhost` / `.local`
//! names (a proxy out on the internet can't reach them, and kiln servers on a LAN worked without
//! one before), and whatever `NO_PROXY` (or the system's exceptions) lists: `*`, host and domain
//! names (`example.com` and `.example.com` match the domain and every name under it), addresses,
//! networks (`10.0.0.0/8`), each optionally with `:port`.

use std::net::IpAddr;
use std::sync::OnceLock;

use anyhow::Context as _;
use hyper_rt::tcp::TcpStream;

/// A proxy to go through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proxy {
    pub host: String,
    pub port: u16,
    /// `user:password` for `Proxy-Authorization: Basic`.
    auth: Option<String>,
}

/// Where outbound connections go: a proxy per scheme, and the exceptions.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Settings {
    https: Option<Proxy>,
    http: Option<Proxy>,
    except: Vec<String>,
}

impl Settings {
    /// From the environment's variables (`get` looks one up).
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Self {
        let var = |names: &[&str]| names.iter().find_map(|n| get(n).filter(|v| !v.trim().is_empty()));
        let all = var(&["ALL_PROXY", "all_proxy"]);
        let https = var(&["HTTPS_PROXY", "https_proxy"]).or_else(|| all.clone()).and_then(|u| parse_url(&u));
        let http = var(&["HTTP_PROXY", "http_proxy"]).or(all).and_then(|u| parse_url(&u));
        let except = var(&["NO_PROXY", "no_proxy"]).map(|v| split_list(&v)).unwrap_or_default();
        Self { https, http, except }
    }

    /// From `scutil --proxy` (macOS's system settings): its HTTPS and HTTP proxies, if enabled,
    /// and its exceptions.
    pub fn from_scutil(text: &str) -> Self {
        let mut keys = std::collections::HashMap::new();
        let mut except = Vec::new();
        let mut in_exceptions = false;
        for line in text.lines().map(str::trim) {
            if line.starts_with("ExceptionsList") {
                in_exceptions = true;
                continue;
            }
            if in_exceptions {
                if line == "}" {
                    in_exceptions = false;
                } else if let Some((_, v)) = line.split_once(" : ") {
                    except.push(v.trim().to_owned());
                }
                continue;
            }
            if let Some((k, v)) = line.split_once(" : ") {
                keys.insert(k.trim().to_owned(), v.trim().to_owned());
            }
        }
        let proxy = |kind: &str| {
            (keys.get(&format!("{kind}Enable")).map(String::as_str) == Some("1"))
                .then(|| Some(Proxy { host: keys.get(&format!("{kind}Proxy"))?.clone(), port: keys.get(&format!("{kind}Port"))?.parse().ok()?, auth: None }))
                .flatten()
                .filter(|p| !p.host.is_empty())
        };
        Self { https: proxy("HTTPS"), http: proxy("HTTP"), except }
    }

    /// The proxy for a connection to `host:port` (`https`: the scheme it speaks), if any.
    pub fn choose(&self, https: bool, host: &str, port: u16) -> Option<&Proxy> {
        let p = if https { self.https.as_ref() } else { self.http.as_ref() }?;
        if always_direct(host) || self.except.iter().any(|e| excepted(e, host, port)) {
            return None;
        }
        Some(p)
    }

    pub fn is_empty(&self) -> bool {
        self.https.is_none() && self.http.is_none()
    }
}

/// This process's settings: the environment's, else (macOS) the system's. Read once.
pub fn settings() -> &'static Settings {
    static S: OnceLock<Settings> = OnceLock::new();
    S.get_or_init(|| {
        let from_env = Settings::from_vars(|n| std::env::var(n).ok());
        if !from_env.is_empty() || !cfg!(target_os = "macos") {
            return from_env;
        }
        std::process::Command::new("scutil")
            .arg("--proxy")
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| Settings::from_scutil(&String::from_utf8_lossy(&o.stdout)))
            .unwrap_or_default()
    })
}

/// `http://[user:password@]host[:port]` (no scheme: `http://`; no port: 80, or 443 for an
/// `https://` proxy, which is refused: kiln speaks to proxies in plain HTTP).
fn parse_url(raw: &str) -> Option<Proxy> {
    let raw = raw.trim();
    let rest = match raw.split_once("://") {
        Some(("http", rest)) => rest,
        Some((other, _)) => {
            tracing::warn!(proxy = %raw, scheme = other, "only http:// proxies are supported; going direct");
            return None;
        }
        None => raw,
    };
    let rest = rest.split('/').next()?;
    let (auth, hostport) = match rest.rsplit_once('@') {
        Some((a, h)) => (Some(percent_decode(a)), h),
        None => (None, rest),
    };
    let (host, port) = split_host_port(hostport);
    let port = match port {
        Some(p) => p.parse().ok()?,
        None => 80,
    };
    (!host.is_empty()).then(|| Proxy { host: host.to_owned(), port, auth })
}

/// `host:port`, `[v6]:port` or a bare host / address.
fn split_host_port(s: &str) -> (&str, Option<&str>) {
    if let Some(rest) = s.strip_prefix('[') {
        return match rest.split_once(']') {
            Some((h, tail)) => (h, tail.strip_prefix(':')),
            None => (rest, None),
        };
    }
    match s.rsplit_once(':') {
        // One colon: a port; more: a bare IPv6 address.
        Some((h, p)) if !h.contains(':') => (h, Some(p)),
        _ => (s, None),
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        let hex = |j: usize| bytes.get(j).and_then(|c| char::from(*c).to_digit(16));
        match (b, hex(i + 1), hex(i + 2)) {
            (b'%', Some(h), Some(l)) => {
                out.push(u8::try_from(h * 16 + l).unwrap_or(b'?'));
                i += 3;
            }
            _ => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn split_list(v: &str) -> Vec<String> {
    v.split([',', ' ', ';']).map(str::trim).filter(|e| !e.is_empty()).map(str::to_owned).collect()
}

/// Names and addresses a proxy on the internet can't reach.
fn always_direct(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']').trim_end_matches('.').to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(a)) => a.is_loopback() || a.is_private() || a.is_link_local() || a.is_unspecified() || (a.octets()[0] == 100 && (a.octets()[1] & 0xc0) == 64),
        Ok(IpAddr::V6(a)) => a.is_loopback() || a.is_unspecified() || (a.segments()[0] & 0xfe00) == 0xfc00 || (a.segments()[0] & 0xffc0) == 0xfe80,
        Err(_) => false,
    }
}

/// Whether exception `entry` (a `NO_PROXY` item or a system exception) covers `host:port`.
fn excepted(entry: &str, host: &str, port: u16) -> bool {
    let entry = entry.trim().to_ascii_lowercase();
    if entry == "*" {
        return true;
    }
    let host = host.trim_start_matches('[').trim_end_matches(']').trim_end_matches('.').to_ascii_lowercase();
    // A port on the entry must match too.
    let (pattern, want_port) = match split_host_port(&entry) {
        (h, Some(p)) if entry.contains('/') || !h.contains('/') => (h.to_owned(), p.parse::<u16>().ok()),
        _ => (entry.clone(), None),
    };
    if want_port.is_some_and(|p| p != port) {
        return false;
    }
    if let Some((net, bits)) = pattern.split_once('/') {
        return in_network(&host, net, bits);
    }
    if let (Ok(a), Ok(b)) = (pattern.parse::<IpAddr>(), host.parse::<IpAddr>()) {
        return a == b;
    }
    let domain = pattern.trim_start_matches("*.").trim_start_matches('.');
    !domain.is_empty() && (host == domain || host.ends_with(&format!(".{domain}")))
}

/// `host` (an address) in `net/bits`; `net` may be short (`169.254/16`).
fn in_network(host: &str, net: &str, bits: &str) -> bool {
    let Ok(addr) = host.parse::<IpAddr>() else { return false };
    let Ok(bits) = bits.parse::<u32>() else { return false };
    let net = if net.contains(':') || net.matches('.').count() == 3 {
        net.to_owned()
    } else {
        let mut parts: Vec<&str> = net.split('.').collect();
        parts.resize(4, "0");
        parts.join(".")
    };
    match (addr, net.parse::<IpAddr>()) {
        (IpAddr::V4(a), Ok(IpAddr::V4(n))) if bits <= 32 => {
            let mask = u32::MAX.checked_shl(32 - bits).unwrap_or(0);
            (u32::from(a) & mask) == (u32::from(n) & mask)
        }
        (IpAddr::V6(a), Ok(IpAddr::V6(n))) if bits <= 128 => {
            let mask = u128::MAX.checked_shl(128 - bits).unwrap_or(0);
            (u128::from(a) & mask) == (u128::from(n) & mask)
        }
        _ => false,
    }
}

/// Ask `proxy` (connected on `stream`) for a tunnel to `host:port`. Afterwards the stream
/// carries that connection as if it had been made directly.
pub async fn tunnel(stream: &TcpStream, proxy: &Proxy, host: &str, port: u16) -> anyhow::Result<()> {
    use base64::Engine as _;
    let target = if host.contains(':') && !host.starts_with('[') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
    let mut req = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nUser-Agent: kiln\r\n");
    if let Some(auth) = &proxy.auth {
        req.push_str(&format!("Proxy-Authorization: Basic {}\r\n", base64::engine::general_purpose::STANDARD.encode(auth)));
    }
    req.push_str("\r\n");
    write_all(stream, req.as_bytes()).await.with_context(|| format!("ask the proxy {}:{} for {target}", proxy.host, proxy.port))?;
    // Its answer, up to the blank line; nothing may follow before this side speaks.
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        anyhow::ensure!(head.len() < 16 * 1024, "the proxy's answer is too long");
        let n = read_some(stream, &mut buf).await.with_context(|| format!("read the proxy's answer for {target}"))?;
        anyhow::ensure!(n > 0, "the proxy {}:{} closed the connection instead of answering", proxy.host, proxy.port);
        head.extend_from_slice(buf.get(..n).unwrap_or_default());
    }
    let end = head.windows(4).position(|w| w == b"\r\n\r\n").map_or(head.len(), |p| p + 4);
    anyhow::ensure!(end == head.len(), "the proxy sent data before the tunnel to {target} was used");
    let text = String::from_utf8_lossy(&head);
    let status = text.lines().next().unwrap_or_default().to_owned();
    let code: u16 = status.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    match code {
        200..=299 => Ok(()),
        407 => anyhow::bail!("the proxy {}:{} wants credentials ({status}): give them in its URL, http://user:password@{}:{}", proxy.host, proxy.port, proxy.host, proxy.port),
        _ => anyhow::bail!("the proxy {}:{} refused a tunnel to {target}: {status}", proxy.host, proxy.port),
    }
}

async fn write_all(stream: &TcpStream, mut data: &[u8]) -> std::io::Result<()> {
    while !data.is_empty() {
        match stream.try_write(data).map_err(crate::rt::to_io)? {
            Some(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Some(n) => data = data.get(n..).unwrap_or_default(),
            None => stream.writable().await.map_err(crate::rt::to_io)?,
        }
    }
    Ok(())
}

async fn read_some(stream: &TcpStream, buf: &mut [u8]) -> std::io::Result<usize> {
    loop {
        match stream.try_read(buf).map_err(crate::rt::to_io)? {
            Some(n) => return Ok(n),
            None => stream.readable().await.map_err(crate::rt::to_io)?,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(list: &[(&str, &str)]) -> Settings {
        let list: Vec<(String, String)> = list.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        Settings::from_vars(|n| list.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone()))
    }

    #[test]
    fn proxies_come_from_the_environment() {
        let s = vars(&[("HTTPS_PROXY", "http://alice:p%40ss@proxy.corp:3128"), ("http_proxy", "proxy.corp"), ("NO_PROXY", "internal.corp, 10.0.0.0/8,.svc,build:8080")]);
        let p = s.choose(true, "kiln.example.com", 443).cloned();
        assert_eq!(p, Some(Proxy { host: "proxy.corp".into(), port: 3128, auth: Some("alice:p@ss".into()) }));
        assert_eq!(s.choose(false, "kiln.example.com", 7070).map(|p| p.port), Some(80));
        // Exceptions: domains and what is under them, networks, ports.
        assert!(s.choose(true, "internal.corp", 443).is_none());
        assert!(s.choose(true, "ci.internal.corp", 443).is_none());
        assert!(s.choose(true, "notinternal.corp", 443).is_some());
        assert!(s.choose(true, "kiln.default.svc", 443).is_none());
        assert!(s.choose(false, "build", 8080).is_none());
        assert!(s.choose(false, "build", 9090).is_some());
        // Never through a proxy: loopback, private networks, .local names.
        for h in ["127.0.0.1", "localhost", "192.168.1.77", "10.2.3.4", "172.20.0.1", "100.101.102.103", "[::1]", "fd00::1", "box.local"] {
            assert!(s.choose(true, h, 443).is_none(), "{h}");
        }
        assert!(s.choose(true, "8.8.8.8", 443).is_some());
        // ALL_PROXY stands in for either; nothing set: direct.
        let s = vars(&[("ALL_PROXY", "http://gw:8080")]);
        assert_eq!(s.choose(true, "a.example", 443).map(|p| p.host.as_str()), Some("gw"));
        assert!(vars(&[]).choose(true, "a.example", 443).is_none());
        assert!(vars(&[("NO_PROXY", "*"), ("HTTPS_PROXY", "http://gw:8080")]).choose(true, "a.example", 443).is_none());
        // Proxies kiln can't speak to are ignored.
        assert!(vars(&[("HTTPS_PROXY", "socks5://gw:1080")]).choose(true, "a.example", 443).is_none());
    }

    #[test]
    fn macos_system_proxies_are_read() {
        let text = "<dictionary> {\n  ExceptionsList : <array> {\n    0 : *.corp\n    1 : 169.254/16\n  }\n  FTPPassive : 1\n  HTTPEnable : 1\n  HTTPPort : 8080\n  HTTPProxy : gw.corp\n  HTTPSEnable : 1\n  HTTPSPort : 8443\n  HTTPSProxy : gws.corp\n  ProxyAutoConfigEnable : 0\n}\n";
        let s = Settings::from_scutil(text);
        assert_eq!(s.choose(true, "github.com", 443).map(|p| (p.host.as_str(), p.port)), Some(("gws.corp", 8443)));
        assert_eq!(s.choose(false, "github.com", 80).map(|p| (p.host.as_str(), p.port)), Some(("gw.corp", 8080)));
        assert!(s.choose(true, "kiln.corp", 443).is_none());
        assert!(!excepted("169.254/16", "169.253.1.1", 80));
        assert!(excepted("169.254/16", "169.254.9.9", 80));
        // Disabled proxies are none.
        assert!(Settings::from_scutil("<dictionary> {\n  HTTPSEnable : 0\n  HTTPSProxy : gws.corp\n  HTTPSPort : 8443\n}\n").is_empty());
    }

    /// A CONNECT through a proxy (a thread here) gives a stream to the target behind it.
    #[test]
    fn a_tunnel_through_a_proxy_reaches_the_target() -> anyhow::Result<()> {
        use std::io::{Read as _, Write as _};
        let target = std::net::TcpListener::bind("127.0.0.1:0")?;
        let target_port = target.local_addr()?.port();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = target.accept() {
                let mut b = [0u8; 64];
                let n = s.read(&mut b).unwrap_or(0);
                let _ = s.write_all(&b.get(..n).unwrap_or_default().iter().rev().copied().collect::<Vec<u8>>());
            }
        });
        let proxy = std::net::TcpListener::bind("127.0.0.1:0")?;
        let proxy_port = proxy.local_addr()?.port();
        let asked = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let seen = std::sync::Arc::clone(&asked);
        std::thread::spawn(move || {
            let Ok((mut client, _)) = proxy.accept() else { return };
            let mut head = Vec::new();
            let mut b = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") && client.read(&mut b).unwrap_or(0) == 1 {
                head.extend_from_slice(&b);
            }
            *seen.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = String::from_utf8_lossy(&head).into_owned();
            let Ok(mut up) = std::net::TcpStream::connect(("127.0.0.1", target_port)) else { return };
            let _ = client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n");
            let mut b = [0u8; 64];
            let n = client.read(&mut b).unwrap_or(0);
            let _ = up.write_all(b.get(..n).unwrap_or_default());
            let n = up.read(&mut b).unwrap_or(0);
            let _ = client.write_all(b.get(..n).unwrap_or_default());
        });
        crate::rt::block_on(async move {
            let p = Proxy { host: "127.0.0.1".into(), port: proxy_port, auth: Some("u:p".into()) };
            let stream = crate::http::io::connect("127.0.0.1", proxy_port).await?;
            tunnel(&stream, &p, "kiln.example", 7070).await?;
            write_all(&stream, b"olleh").await?;
            let mut buf = [0u8; 16];
            let n = read_some(&stream, &mut buf).await?;
            assert_eq!(buf.get(..n), Some(&b"hello"[..]));
            Ok(())
        })?;
        let asked = asked.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        assert!(asked.starts_with("CONNECT kiln.example:7070 HTTP/1.1\r\n"), "{asked}");
        assert!(asked.contains("Proxy-Authorization: Basic dTpw\r\n"), "{asked}");
        Ok(())
    }
}
