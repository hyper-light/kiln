//! A runner's **front door**: the machine people's phones and laptops can reach, made into kiln's
//! address on the LAN. Configured per runner from the UI, stored by the server, run by the runner:
//!
//! * **DNS** ([`dns`], [`zone`]): answers A/AAAA for a domain, any name the admin chooses
//!   (`kiln.dev`), its aliases and `*.<domain>` with the runner's LAN address facing the asker (or
//!   configured addresses). As a *resolver* it forwards every other query to the system's upstream
//!   resolvers, so a phone or a router can use the runner as its DNS server; as an *authoritative*
//!   server it refuses them, for a router that forwards only the domain (conditional forwarding).
//! * **HTTPS** ([`proxy`]): accepts TLS for those names with a self-signed certificate from the
//!   kiln CA (issued for the runner's CSR, renewed well before it expires, or at once when asked)
//!   or one uploaded for it, and pipes the bytes both ways to the kiln server (the runner's own
//!   `--server` URL by default), so WebSockets and streams work unchanged. Plain HTTP on another
//!   port redirects to it.
//! * **`.local`**: answers `<name>.local` through kiln's multicast DNS responder: no setup at all.
//!
//! The server pushes [`Push`] (settings, and a certificate the CA issued or one uploaded) with
//! `ServerMsg::FrontDoor`; the runner reports [`Status`] with `AgentMsg::FrontDoorStatus`. Both are
//! JSON, so either side may add fields. See `docs/front-door.md`.

pub mod certs;
pub mod control;
pub mod dns;
pub mod mobileconfig;
pub mod netinfo;
pub mod proxy;
pub mod runner;
pub mod tailnet;
pub mod zone;

use std::net::IpAddr;

use serde::{Deserialize, Serialize};

pub use crate::pki::CertInfo;

pub const DNS_PORT: u16 = 53;
pub const HTTPS_PORT: u16 = 443;
pub const HTTP_PORT: u16 = 80;
/// The `.local` name used until another is chosen.
pub const MDNS_NAME: &str = "kiln";
const MAX_ALIASES: usize = 8;
const MAX_UPSTREAM: usize = 4;
const MAX_ADDRESSES: usize = 8;

// ------------------------------------------------------------------------------ settings

/// A runner's front door, as the admin set it (`PUT /api/runners/:name/front-door`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The name devices use, any the admin chooses (`kiln.dev`, `kiln.home`); every name under
    /// it (`*.kiln.dev`) answers too.
    pub domain: String,
    /// More names for the same front door, e.g. `ci.home`.
    pub aliases: Vec<String>,
    pub dns: Dns,
    pub https: Https,
    pub cert: Cert,
    pub mdns: Mdns,
    /// The front door on the tailnet too (Tailscale), and the tailnet's DNS for its names.
    /// Left out of the JSON while off, so settings that do not use it fingerprint as before.
    #[serde(skip_serializing_if = "tailnet::Tailnet::is_off")]
    pub tailscale: tailnet::Tailnet,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DnsMode {
    /// Answers the domain and forwards every other query upstream: a whole network's DNS server.
    #[default]
    Resolver,
    /// Answers the domain only and refuses the rest: for a router that forwards just the domain.
    Authoritative,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Dns {
    pub enabled: bool,
    pub port: u16,
    pub mode: DnsMode,
    /// Resolvers to forward to (`192.168.1.1`, `[fd00::1]:53`). Empty: the system's.
    pub upstream: Vec<String>,
    /// Addresses to answer with. Empty: the runner's address on the interface facing the asker.
    pub addresses: Vec<String>,
}

impl Default for Dns {
    fn default() -> Self {
        Self { enabled: false, port: DNS_PORT, mode: DnsMode::Resolver, upstream: Vec::new(), addresses: Vec::new() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Https {
    pub enabled: bool,
    pub port: u16,
    /// Plain HTTP that redirects to HTTPS; 0: none.
    pub http_port: u16,
    /// Where requests go (`http://192.168.1.77:7070`). Empty: the runner's own `--server` URL.
    pub upstream: String,
}

impl Default for Https {
    fn default() -> Self {
        Self { enabled: false, port: HTTPS_PORT, http_port: HTTP_PORT, upstream: String::new() }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CertMode {
    /// Self-signed: issued by the kiln CA (kiln's own root) for the front door's names and the
    /// runner's addresses, installed and renewed by kiln.
    #[default]
    Kiln,
    /// A certificate and key uploaded for it (`POST /api/runners/:name/front-door/cert`).
    Custom,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Cert {
    pub mode: CertMode,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Mdns {
    pub enabled: bool,
    /// `kiln` answers as `kiln.local`.
    pub name: String,
}

impl Default for Mdns {
    fn default() -> Self {
        Self { enabled: false, name: MDNS_NAME.to_owned() }
    }
}

/// A host name a front door may answer for: lowercase letters, digits and hyphens in labels of
/// at most 63, with at least one dot, at most 253 in all; not an address, not under `.local`
/// (multicast DNS's), `localhost`, or the server's own TLS identity.
pub fn host_name(s: &str) -> Result<String, String> {
    let n = s.trim().trim_end_matches('.').to_ascii_lowercase();
    if n.is_empty() {
        return Err("a name is empty".to_owned());
    }
    if n.parse::<IpAddr>().is_ok() {
        return Err(format!("{n} is an address, not a name"));
    }
    if n.len() > 253 {
        return Err(format!("{n} is longer than 253 characters"));
    }
    let labels: Vec<&str> = n.split('.').collect();
    for l in &labels {
        let ok = (1..=63).contains(&l.len())
            && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !l.starts_with('-')
            && !l.ends_with('-');
        if !ok {
            return Err(format!("{n} is not a host name (letters, digits and hyphens between dots)"));
        }
    }
    if labels.len() < 2 {
        return Err(format!("{n} needs a dot, like {n}.dev"));
    }
    if labels.last().is_some_and(|t| t.bytes().all(|b| b.is_ascii_digit())) {
        return Err(format!("{n} ends in a number, which no top-level domain does"));
    }
    if n == "local" || n.ends_with(".local") {
        return Err(format!("{n}: names under .local belong to multicast DNS; turn on the .local name instead"));
    }
    if n == "localhost" || n.ends_with(".localhost") {
        return Err(format!("{n} always means this very device"));
    }
    if n == crate::pki::SERVER_NAME {
        return Err(format!("{n} is the name runners check the server by"));
    }
    Ok(n)
}

/// A single `.local` label: letters, digits and hyphens, at most 63.
pub fn local_label(s: &str) -> Result<String, String> {
    let l = s.trim().trim_end_matches('.').to_ascii_lowercase();
    let l = l.strip_suffix(".local").unwrap_or(&l).to_owned();
    let ok = (1..=63).contains(&l.len())
        && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !l.starts_with('-')
        && !l.ends_with('-');
    if ok { Ok(l) } else { Err(format!("{s:?} is not a .local name (letters, digits and hyphens)")) }
}

/// A URL kiln can forward to: `http://` or `https://`, a host, an optional port, no path.
pub fn upstream_url(s: &str) -> Result<String, String> {
    let s = s.trim().trim_end_matches('/');
    let uri: http::Uri = s.parse().map_err(|_| format!("{s:?} is not a URL"))?;
    let scheme = uri.scheme_str().unwrap_or("");
    if scheme != "http" && scheme != "https" {
        return Err(format!("{s}: the upstream is an http:// or https:// URL"));
    }
    if uri.host().is_none_or(str::is_empty) {
        return Err(format!("{s}: no host"));
    }
    if uri.path_and_query().is_some_and(|p| p.as_str() != "/" && !p.as_str().is_empty()) {
        return Err(format!("{s}: the upstream is a server, without a path"));
    }
    Ok(s.to_owned())
}

impl Settings {
    /// The settings checked and tidied (names lowercased, duplicates gone), or why they can't be.
    pub fn validated(&self) -> Result<Settings, String> {
        let mut s = self.clone();
        s.domain = if self.domain.trim().is_empty() { String::new() } else { host_name(&self.domain).map_err(|e| format!("Domain: {e}"))? };
        let mut aliases: Vec<String> = Vec::new();
        for a in self.aliases.iter().filter(|a| !a.trim().is_empty()) {
            let a = host_name(a).map_err(|e| format!("Alias: {e}"))?;
            if a != s.domain && !aliases.contains(&a) {
                aliases.push(a);
            }
        }
        if aliases.len() > MAX_ALIASES {
            return Err(format!("At most {MAX_ALIASES} aliases"));
        }
        if !aliases.is_empty() && s.domain.is_empty() {
            return Err("Aliases need a domain first".to_owned());
        }
        s.aliases = aliases;
        if (s.dns.enabled || s.https.enabled) && s.domain.is_empty() && !(s.https.enabled && s.mdns.enabled) {
            return Err("Choose a domain for DNS and HTTPS: any name, like kiln.dev".to_owned());
        }
        if s.dns.enabled && s.domain.is_empty() {
            return Err("DNS answers for a domain: choose one, like kiln.dev".to_owned());
        }
        // DNS
        if s.dns.port == 0 {
            return Err("DNS needs a port (53 is the one devices ask)".to_owned());
        }
        let mut upstream = Vec::new();
        for u in self.dns.upstream.iter().map(|u| u.trim()).filter(|u| !u.is_empty()) {
            let a = crate::discovery::unicast::parse_server(u).ok_or_else(|| format!("Upstream resolver: {u:?} is not an address (192.168.1.1, [fd00::1]:53)"))?;
            let a = a.to_string();
            if !upstream.contains(&a) {
                upstream.push(a);
            }
        }
        if upstream.len() > MAX_UPSTREAM {
            return Err(format!("At most {MAX_UPSTREAM} upstream resolvers"));
        }
        s.dns.upstream = upstream;
        let mut addresses = Vec::new();
        for a in self.dns.addresses.iter().map(|a| a.trim()).filter(|a| !a.is_empty()) {
            let ip: IpAddr = a.parse().map_err(|_| format!("Answer with: {a:?} is not an IP address"))?;
            if ip.is_unspecified() || ip.is_multicast() {
                return Err(format!("Answer with: {ip} is not a host's address"));
            }
            let ip = ip.to_string();
            if !addresses.contains(&ip) {
                addresses.push(ip);
            }
        }
        if addresses.len() > MAX_ADDRESSES {
            return Err(format!("At most {MAX_ADDRESSES} addresses to answer with"));
        }
        s.dns.addresses = addresses;
        // HTTPS
        if s.https.port == 0 {
            return Err("HTTPS needs a port (443 is the one browsers use)".to_owned());
        }
        if s.https.http_port == s.https.port {
            return Err("The HTTP redirect needs a port of its own (80, or 0 for none)".to_owned());
        }
        s.https.upstream = if self.https.upstream.trim().is_empty() { String::new() } else { upstream_url(&self.https.upstream).map_err(|e| format!("Upstream: {e}"))? };
        if s.dns.enabled && s.https.enabled && (s.dns.port == s.https.port || s.dns.port == s.https.http_port) {
            return Err("DNS and HTTPS need different ports (DNS answers over TCP too)".to_owned());
        }
        // .local
        s.mdns.name = if self.mdns.name.trim().is_empty() { MDNS_NAME.to_owned() } else { local_label(&self.mdns.name).map_err(|e| format!(".local name: {e}"))? };
        Ok(s)
    }

    /// Whether anything is on.
    pub fn any(&self) -> bool {
        self.dns.enabled || self.https.enabled || self.mdns.enabled
    }

    /// The names DNS answers for, exactly (`*.<domain>` answers too, see [`zone::Zone`]).
    pub fn dns_names(&self) -> Vec<String> {
        std::iter::once(&self.domain).chain(&self.aliases).filter(|n| !n.is_empty()).cloned().collect()
    }

    /// `<name>.local`, while it is on.
    pub fn local_name(&self) -> Option<String> {
        self.mdns.enabled.then(|| format!("{}.local", self.mdns.name))
    }

    /// The host names HTTPS serves: the domain and `*.<domain>`, the aliases, and the `.local`
    /// name when that is on. A certificate for the front door carries these.
    pub fn cert_names(&self) -> Vec<String> {
        let mut v = Vec::new();
        if !self.domain.is_empty() {
            v.push(self.domain.clone());
            v.push(format!("*.{}", self.domain));
        }
        v.extend(self.aliases.iter().cloned());
        v.extend(self.local_name());
        v
    }

    /// Whether a request for `host` (no port) came through this front door's HTTPS.
    pub fn serves(&self, host: &str) -> bool {
        self.https.enabled && crate::pki::covers(&self.cert_names(), host)
    }

    /// `https://<name>` for each name HTTPS serves (with the port when it is not 443), wildcards
    /// left out: where browsers find kiln through this front door.
    pub fn origins(&self) -> Vec<String> {
        if !self.https.enabled {
            return Vec::new();
        }
        let port = if self.https.port == HTTPS_PORT { String::new() } else { format!(":{}", self.https.port) };
        self.cert_names().into_iter().filter(|n| !n.starts_with("*.")).map(|n| format!("https://{n}{port}")).collect()
    }

    /// A short fingerprint of these settings, so a status can say which ones it is for.
    pub fn fingerprint(&self) -> String {
        let json = serde_json::to_string(self).unwrap_or_default();
        crate::util::sha256_hex(json.as_bytes()).chars().take(16).collect()
    }

    /// One line for the audit log: what is on, where.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.domain.is_empty() {
            parts.push(std::iter::once(self.domain.clone()).chain(self.aliases.iter().cloned()).collect::<Vec<_>>().join(", "));
        }
        if self.dns.enabled {
            let mode = match self.dns.mode {
                DnsMode::Resolver => "resolver",
                DnsMode::Authoritative => "authoritative",
            };
            parts.push(format!("DNS :{} ({mode})", self.dns.port));
        }
        if self.https.enabled {
            let cert = match self.cert.mode {
                CertMode::Kiln => "self-signed certificate",
                CertMode::Custom => "own certificate",
            };
            let http = if self.https.http_port == 0 { String::new() } else { format!(", HTTP :{} redirects", self.https.http_port) };
            parts.push(format!("HTTPS :{} ({cert}{http})", self.https.port));
        }
        if let Some(l) = self.local_name() {
            parts.push(l);
        }
        if self.tailscale.enabled {
            parts.push("tailnet".to_owned());
        }
        if parts.len() <= usize::from(!self.domain.is_empty()) {
            parts.push("off".to_owned());
        }
        parts.join(" · ")
    }
}

// ------------------------------------------------------------------------------ server → runner

/// What the server sends a runner (`ServerMsg::FrontDoor`).
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Push {
    pub settings: Settings,
    /// Another runner process of this machine runs its front door (it is the newest): stop every
    /// listener here and leave what is stored alone.
    pub standby: bool,
    /// The kiln CA's certificate for the CSR this runner sent (PEM).
    pub issued: Option<String>,
    /// A certificate and key uploaded for this front door: stored with mode 0600, never logged,
    /// never sent back.
    pub custom: Option<CustomCert>,
    /// Renew the self-signed certificate now (`POST /api/runners/:name/front-door/cert/renew`):
    /// a new key, and a CSR for it at once. The certificate in use serves until the new one is in.
    pub renew: bool,
    /// The clusters this one is peered with, which the front door forwards to itself
    /// (`crate::peers`); `None` from a server that has none to say.
    pub peers: Option<crate::peers::forward::Table>,
}

impl std::fmt::Debug for Push {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Push")
            .field("settings", &self.settings)
            .field("standby", &self.standby)
            .field("issued", &self.issued.as_ref().map(String::len))
            .field("custom", &self.custom)
            .field("renew", &self.renew)
            .finish()
    }
}

/// A PEM certificate chain and its PEM private key.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomCert {
    pub cert_pem: String,
    pub key_pem: String,
}

impl std::fmt::Debug for CustomCert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CustomCert").field("cert_pem", &self.cert_pem.len()).field("key_pem", &"<redacted>").finish()
    }
}

// ------------------------------------------------------------------------------ runner → server

/// How a part of the front door is doing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Not turned on.
    #[default]
    Off,
    /// Listening, working.
    On,
    /// On, but waiting for something (a certificate, the other runner process of the machine).
    Waiting,
    /// On, and failing: see `error` and the status's fixes.
    Error,
}

/// One listener (DNS, HTTPS, the HTTP redirect, the `.local` name).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Listener {
    pub state: State,
    /// Where it listens: `192.168.1.77:53`, `[::]:443`, `kiln.local`.
    pub on: Vec<String>,
    /// What went wrong, or what it waits for.
    pub error: Option<String>,
    /// HTTPS: where requests go; DNS: the resolvers it forwards to (resolver mode).
    pub to: Vec<String>,
    /// Queries answered (DNS), connections taken (HTTPS), since it started.
    pub served: u64,
    /// What fixes its error, when that is one-time setup on the machine (also in the status's
    /// `fixes`).
    pub fix: Option<Fix>,
}

/// The certificate HTTPS serves.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CertStatus {
    pub mode: CertMode,
    pub state: State,
    pub info: Option<CertInfo>,
    /// Issued by the kiln CA.
    pub kiln: bool,
    /// Names the front door serves that the certificate does not cover.
    pub missing: Vec<String>,
    pub error: Option<String>,
    /// The chain it serves, its certificates only ([`crate::pki::public_chain_pem`]): never a
    /// key. The server keeps it for downloads and leaves it out of the view.
    pub pem: Option<String>,
    /// A new self-signed certificate was asked for (renew now) and is not in yet.
    pub renewing: bool,
}

/// Something to run once on the machine (the front door cannot do it as the user it runs as).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Fix {
    /// Why, in a sentence.
    pub why: String,
    /// The commands, in order.
    pub commands: Vec<String>,
}

/// What a runner reports (`AgentMsg::FrontDoorStatus`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Status {
    /// The fingerprint of the settings this is for ([`Settings::fingerprint`]).
    pub settings: String,
    /// This process stands by while another one of the machine runs the front door.
    pub standby: bool,
    pub os: String,
    /// The runner's own `--server` URL: where HTTPS forwards unless the settings say otherwise.
    pub server: String,
    /// The runner's LAN addresses.
    pub addresses: Vec<String>,
    pub dns: Listener,
    pub https: Listener,
    pub http: Listener,
    pub mdns: Listener,
    pub cert: CertStatus,
    /// A CSR for the kiln CA: the front door needs a certificate for `want`.
    pub csr: Option<String>,
    pub want: Vec<String>,
    /// When the CSR was (last) asked (unix seconds): asked again every few minutes until answered.
    pub asked: i64,
    pub fixes: Vec<Fix>,
    /// The front door on the tailnet, while `tailscale.enabled`.
    pub tailscale: Option<tailnet::TailnetStatus>,
}

/// A port for a test to listen on (TCP and UDP): below Linux's ephemeral range, where no
/// connection's own end and no other test's `bind(0)` lands, and never one this process handed out
/// before. Taking a port from `bind(0)` and binding it again races every test running alongside.
#[cfg(test)]
pub(crate) fn test_port() -> std::io::Result<u16> {
    use std::sync::atomic::{AtomicU16, Ordering};
    static NEXT: AtomicU16 = AtomicU16::new(0);
    let start = 20_000 + u16::try_from(std::process::id() % 10_000).unwrap_or(0);
    let _ = NEXT.compare_exchange(0, start, Ordering::SeqCst, Ordering::SeqCst);
    for _ in 0..500 {
        let port = NEXT.fetch_add(1, Ordering::SeqCst);
        let free = std::net::TcpListener::bind(("0.0.0.0", port)).is_ok() && std::net::UdpSocket::bind(("0.0.0.0", port)).is_ok();
        if free {
            return Ok(port);
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::AddrInUse, "no free port for a test"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on() -> Settings {
        Settings {
            domain: "Kiln.Home.".into(),
            aliases: vec!["ci.home".into(), "kiln.home".into(), " ".into(), "CI.home".into()],
            dns: Dns { enabled: true, upstream: vec!["192.168.1.254".into(), "[fd00::1]:5353".into()], addresses: vec!["192.168.1.77".into()], ..Dns::default() },
            https: Https { enabled: true, ..Https::default() },
            cert: Cert::default(),
            mdns: Mdns { enabled: true, name: "Kiln.local".into() },
            ..Settings::default()
        }
    }

    #[test]
    fn settings_validate_and_tidy() -> anyhow::Result<()> {
        let s = on().validated().map_err(anyhow::Error::msg)?;
        assert_eq!(s.domain, "kiln.home");
        assert_eq!(s.aliases, ["ci.home"]);
        assert_eq!(s.dns.upstream, ["192.168.1.254:53", "[fd00::1]:5353"]);
        assert_eq!(s.mdns.name, "kiln");
        assert_eq!(s.cert_names(), ["kiln.home", "*.kiln.home", "ci.home", "kiln.local"]);
        assert_eq!(s.dns_names(), ["kiln.home", "ci.home"]);
        assert_eq!(s.origins(), ["https://kiln.home", "https://ci.home", "https://kiln.local"]);
        assert!(s.serves("x.kiln.home") && s.serves("kiln.local") && !s.serves("kiln.lan"));
        assert_eq!(s.summary(), "kiln.home, ci.home · DNS :53 (resolver) · HTTPS :443 (self-signed certificate, HTTP :80 redirects) · kiln.local");
        // Validating again changes nothing.
        assert_eq!(s.validated().map_err(anyhow::Error::msg)?, s);
        let off = Settings::default().validated().map_err(anyhow::Error::msg)?;
        assert!(!off.any() && off.origins().is_empty());
        assert_eq!(off.summary(), "off");
        let other = Settings { https: Https { enabled: true, port: 8443, ..Https::default() }, ..s.clone() };
        assert_eq!(other.origins().first().map(String::as_str), Some("https://kiln.home:8443"));
        assert_ne!(other.fingerprint(), s.fingerprint());
        Ok(())
    }

    #[test]
    fn bad_settings_say_why() {
        let bad = |f: &dyn Fn(&mut Settings)| {
            let mut s = on();
            f(&mut s);
            s.validated().err().unwrap_or_default()
        };
        assert!(bad(&|s| s.domain = "kiln.local".into()).contains(".local"));
        assert!(bad(&|s| s.domain = "kiln".into()).contains("needs a dot"));
        assert!(bad(&|s| s.domain = "192.168.1.77".into()).contains("address"));
        assert!(bad(&|s| s.domain = "kiln-server.home".into()).is_empty(), "a name merely like it is fine");
        assert!(bad(&|s| s.domain = "kiln_ci.home".into()).contains("not a host name"));
        assert!(bad(&|s| s.domain = "-kiln.home".into()).contains("not a host name"));
        assert!(bad(&|s| s.domain = "kiln.123".into()).contains("number"));
        assert!(bad(&|s| s.domain = String::new()).contains("domain"));
        assert!(bad(&|s| s.aliases = vec!["ci.local".into()]).starts_with("Alias"));
        assert!(bad(&|s| s.dns.upstream = vec!["resolver.example".into()]).starts_with("Upstream resolver"));
        assert!(bad(&|s| s.dns.addresses = vec!["224.0.0.1".into()]).starts_with("Answer with"));
        assert!(bad(&|s| s.dns.port = 0).contains("port"));
        assert!(bad(&|s| s.https.http_port = 443).contains("port of its own"));
        assert!(bad(&|s| s.dns.port = 443).contains("different ports"));
        assert!(bad(&|s| s.https.upstream = "ftp://x".into()).starts_with("Upstream"));
        assert!(bad(&|s| s.https.upstream = "http://192.168.1.77:7070/ui".into()).contains("path"));
        assert!(bad(&|s| s.https.upstream = "http://192.168.1.77:7070/".into()).is_empty());
        assert!(bad(&|s| s.mdns.name = "my kiln".into()).starts_with(".local"));
        // HTTPS for just the .local name needs no domain.
        let local_only = Settings { https: Https { enabled: true, ..Https::default() }, mdns: Mdns { enabled: true, name: "kiln".into() }, ..Settings::default() };
        assert_eq!(local_only.validated().map(|s| s.cert_names()), Ok(vec!["kiln.local".to_owned()]));
    }

    #[test]
    fn settings_and_status_round_trip_as_json() -> anyhow::Result<()> {
        let s = on().validated().map_err(anyhow::Error::msg)?;
        let push = Push { settings: s.clone(), standby: false, issued: Some("PEM".into()), custom: Some(CustomCert { cert_pem: "C".into(), key_pem: "SECRET KEY".into() }), renew: true, peers: None };
        let json = serde_json::to_string(&push)?;
        let back: Push = serde_json::from_str(&json)?;
        assert_eq!(back.settings, s);
        assert!(back.renew);
        // A push from a server that knows nothing of renewing renews nothing.
        assert!(!serde_json::from_str::<Push>(r#"{"settings":{"domain":"kiln.dev"}}"#)?.renew);
        assert_eq!(back.custom.as_ref().map(|c| c.key_pem.as_str()), Some("SECRET KEY"));
        // Debug output (logs) never carries the key.
        assert!(!format!("{back:?}").contains("SECRET"));
        // Fields either side does not know yet are ignored; missing ones take their defaults.
        let older: Settings = serde_json::from_str(r#"{"domain":"kiln.home","dns":{"enabled":true},"future":1}"#)?;
        assert_eq!(older.dns.port, DNS_PORT);
        assert_eq!(older.https.http_port, HTTP_PORT);
        assert_eq!(older.mdns.name, MDNS_NAME);
        assert_eq!(serde_json::to_value(DnsMode::Authoritative)?, serde_json::json!("authoritative"));
        assert_eq!(serde_json::to_value(CertMode::Custom)?, serde_json::json!("custom"));
        let status = Status {
            settings: s.fingerprint(),
            dns: Listener { state: State::On, on: vec!["192.168.1.77:53".into()], ..Listener::default() },
            fixes: vec![Fix { why: "ports".into(), commands: vec!["sudo sysctl -w net.ipv4.ip_unprivileged_port_start=53".into()] }],
            ..Status::default()
        };
        assert_eq!(serde_json::from_str::<Status>(&serde_json::to_string(&status)?)?, status);
        assert_eq!(serde_json::to_value(State::Waiting)?, serde_json::json!("waiting"));
        Ok(())
    }

    /// The domain is any name the admin chooses: `kiln.dev`, a name on a public suffix, a deep
    /// one, an internationalized one (as its `xn--` form), not only `.home` or `.lan`.
    #[test]
    fn any_name_is_a_domain() -> anyhow::Result<()> {
        let long_label = "k".repeat(63);
        for (asked, kept) in [
            ("kiln.dev", "kiln.dev"),
            ("Kiln.Dev.", "kiln.dev"),
            (" kiln.dev ", "kiln.dev"),
            ("kiln.app", "kiln.app"),
            ("ci.example.com", "ci.example.com"),
            ("kiln.internal", "kiln.internal"),
            ("kiln.home.arpa", "kiln.home.arpa"),
            ("my-kiln.io", "my-kiln.io"),
            ("a.b.c.kiln.dev", "a.b.c.kiln.dev"),
            ("xn--kln-0na.dev", "xn--kln-0na.dev"),
            ("kiln.lan", "kiln.lan"),
            ("kiln.home", "kiln.home"),
            ("kiln.x1", "kiln.x1"),
        ] {
            assert_eq!(host_name(asked).as_deref(), Ok(kept), "{asked:?}");
        }
        assert_eq!(host_name(&format!("{long_label}.dev")).map(|n| n.len()), Ok(67));
        assert!(host_name(&format!("{long_label}k.dev")).is_err(), "a label of 64");
        assert!(host_name("kiln").err().is_some_and(|e| e.contains("like kiln.dev")));
        assert!(host_name("kiln dev").is_err() && host_name("kiln..dev").is_err() && host_name("*.kiln.dev").is_err());

        // kiln.dev as a front door: DNS, HTTPS, its wildcard, an alias under it.
        let s = Settings {
            domain: "kiln.dev".into(),
            aliases: vec!["ci.kiln.dev".into(), "kiln.example.com".into()],
            dns: Dns { enabled: true, ..Dns::default() },
            https: Https { enabled: true, ..Https::default() },
            ..Settings::default()
        }
        .validated()
        .map_err(anyhow::Error::msg)?;
        assert_eq!(s.cert_names(), ["kiln.dev", "*.kiln.dev", "ci.kiln.dev", "kiln.example.com"]);
        assert_eq!(s.origins(), ["https://kiln.dev", "https://ci.kiln.dev", "https://kiln.example.com"]);
        assert!(s.serves("kiln.dev") && s.serves("www.kiln.dev") && s.serves("kiln.example.com") && !s.serves("kiln.home"));
        assert_eq!(s.summary(), "kiln.dev, ci.kiln.dev, kiln.example.com · DNS :53 (resolver) · HTTPS :443 (self-signed certificate, HTTP :80 redirects)");
        let none = Settings { dns: Dns { enabled: true, ..Dns::default() }, ..Settings::default() };
        assert!(none.validated().err().is_some_and(|e| e.contains("kiln.dev")), "the example is kiln.dev");
        Ok(())
    }
}
