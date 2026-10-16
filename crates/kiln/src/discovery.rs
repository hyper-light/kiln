//! Home-network discovery: kiln servers (and agents started with `--discover`) announce themselves
//! with multicast DNS service discovery as `_kiln._tcp.local.` (RFC 6762, RFC 6763) and keep a
//! table of the other kiln instances they hear. A server shows that table in the UI ("On this
//! network"), and `kiln join` uses it to find a server without being told its address.
//!
//! * **Sockets.** One UDP socket per address family on port 5353, with `SO_REUSEADDR` and
//!   `SO_REUSEPORT` because avahi or mDNSResponder usually own that port too, joined to
//!   224.0.0.251 / ff02::fb on every LAN interface, then adopted by hyper-rt (readiness-based
//!   `recv_from`, non-blocking sends). A duplicate handle of the same socket picks the outgoing
//!   interface for each send. Multicast loopback stays on, so instances on one host hear each other;
//!   each ignores its own packets by instance id.
//! * **Announcing.** The full record set (PTR, SRV, TXT, A, and AAAA when the HTTP server listens
//!   on IPv6) at start, one and two seconds later, then every 30 s, separately on each interface
//!   with that interface's addresses. Queries are answered: shared answers by multicast after a
//!   20–120 ms delay and at most once a second per interface, legacy (non-5353 source port) and QU
//!   queries by unicast. Known answers suppress the PTR answer. Goodbyes (TTL 0) go out when the
//!   engine stops.
//! * **Browsing.** Queries for `_kiln._tcp.local.` at start (with known answers), and every
//!   response heard is folded into the [`Table`]. An entry expires when its records' TTL (120 s,
//!   four missed announcements) runs out, or at once on a goodbye.
//! * **Names.** Instances are `kiln <role> on <host> (<id8>)` and their host names
//!   `kiln-<id8>.local`, both from a random instance id kept on disk, so no probing or conflict
//!   resolution is needed and kiln never claims the host's own `.local` name (avahi's).

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use futures::FutureExt as _;
use parking_lot::Mutex;
use serde::Serialize;

use crate::dns::{self, Message, Name, Question, RData, Record};
use crate::rt::{Notify, Shutdown};

/// The DNS-SD service type kiln announces.
pub const SERVICE: &str = "_kiln._tcp.local";
/// DNS-SD service type enumeration (RFC 6763 §9).
const SERVICE_TYPES: &str = "_services._dns-sd._udp.local";
pub const MDNS_PORT: u16 = 5353;
pub const GROUP_V4: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
pub const GROUP_V6: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0xfb);
/// TTL of every record kiln announces: four missed announcements and an entry is gone.
pub const TTL: u32 = 120;
pub const ANNOUNCE_EVERY: Duration = Duration::from_secs(30);
/// Legacy (one-shot, non-5353) queriers get short TTLs (RFC 6762 §6.7).
const LEGACY_TTL: u32 = 10;
const SWEEP_EVERY: Duration = Duration::from_secs(5);
const INTERFACES_EVERY: Duration = Duration::from_secs(30);
/// Most instances the table holds; the one closest to expiring makes room.
const MAX_INSTANCES: usize = 256;
/// Most instances taken from one packet.
const MAX_PER_PACKET: usize = 32;

fn service() -> Name {
    Name::parse(SERVICE)
}

// ------------------------------------------------------------------------------ who we are

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Server,
    Agent,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Agent => "agent",
        }
    }
}

/// This instance's stable random id, kept in `<dir>/instance.id` (created on first use).
pub fn instance_id(dir: &Path) -> String {
    let path = dir.join("instance.id");
    if let Ok(s) = std::fs::read_to_string(&path) {
        let s = s.trim();
        if (16..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return s.to_ascii_lowercase();
        }
    }
    let id = hex::encode(rand::random::<[u8; 16]>());
    if let Err(e) = crate::util::write_secret(&path, id.as_bytes(), 0o644) {
        tracing::warn!(error = %e, path = %path.display(), "cannot store the instance id; it changes on restart");
    }
    id
}

/// The first 16 hex digits of the SHA-256 of a CA certificate (DER): the `ca` TXT value.
pub fn ca_fingerprint(ca_der: &[u8]) -> String {
    crate::util::sha256_hex(ca_der).chars().take(16).collect()
}

/// What this instance announces about itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Advert {
    pub id: String,
    /// The host name.
    pub name: String,
    pub role: Role,
    pub version: String,
    /// The HTTP port (0 for agents, which serve no HTTP).
    pub port: u16,
    /// The QUIC port (0 when there is none).
    pub quic: u16,
    /// Servers: the fingerprint of their CA ([`ca_fingerprint`]).
    pub ca: Option<String>,
    /// Agents: the fingerprint of the CA of the server they are enrolled with.
    pub enrolled: Option<String>,
    pub os: String,
    pub arch: String,
}

impl Advert {
    pub fn new(id: String, role: Role, port: u16, quic: u16) -> Self {
        Self {
            id,
            name: crate::config::Config::hostname(),
            role,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            port,
            quic,
            ca: None,
            enrolled: None,
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
        }
    }

    fn short_id(&self) -> &str {
        self.id.get(..8).unwrap_or(&self.id)
    }

    /// The DNS-SD instance name: `kiln server on adacachy (1a2b3c4d)`, at most 63 bytes.
    pub fn instance_label(&self) -> String {
        let prefix = format!("kiln {} on ", self.role.as_str());
        let suffix = format!(" ({})", self.short_id());
        let room = 63usize.saturating_sub(prefix.len() + suffix.len());
        format!("{prefix}{}{suffix}", crate::util::truncate(&self.name, room))
    }

    fn instance_name(&self) -> Name {
        Name::child(&self.instance_label(), &service())
    }

    fn host_name(&self) -> Name {
        Name::from_labels([format!("kiln-{}", self.short_id()), "local".to_owned()])
    }

    fn txt(&self) -> Vec<Vec<u8>> {
        let quic = self.quic.to_string();
        let mut pairs = vec![
            ("id", self.id.as_str()),
            ("name", self.name.as_str()),
            ("role", self.role.as_str()),
            ("version", self.version.as_str()),
            ("quic", quic.as_str()),
            ("os", self.os.as_str()),
            ("arch", self.arch.as_str()),
        ];
        if let Some(ca) = &self.ca {
            pairs.push(("ca", ca));
        }
        if let Some(e) = &self.enrolled {
            pairs.push(("enrolled", e));
        }
        dns::txt(&pairs)
    }
}

/// Which of our records a message carries as answers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Kinds {
    ptr: bool,
    srv: bool,
    txt: bool,
    addr: bool,
    /// The service type enumeration PTR.
    types: bool,
}

impl Kinds {
    const ALL: Self = Self { ptr: true, srv: true, txt: true, addr: true, types: false };

    fn any(self) -> bool {
        self.ptr || self.srv || self.txt || self.addr || self.types
    }

    fn union(self, o: Self) -> Self {
        Self {
            ptr: self.ptr || o.ptr,
            srv: self.srv || o.srv,
            txt: self.txt || o.txt,
            addr: self.addr || o.addr,
            types: self.types || o.types,
        }
    }
}

/// A response carrying `answer` for `a`, plus the related records as additionals (RFC 6763 §12),
/// with `v4`/`v6` as its addresses.
fn reply(a: &Advert, answer: Kinds, ttl: u32, cache_flush: bool, v4: &[Ipv4Addr], v6: &[Ipv6Addr]) -> Message {
    let service = service();
    let instance = a.instance_name();
    let host = a.host_name();
    let ptr = Record::new(service.clone(), ttl, false, RData::Ptr(instance.clone()));
    let srv = Record::new(instance.clone(), ttl, cache_flush, RData::Srv { priority: 0, weight: 0, port: a.port, target: host.clone() });
    let txt = Record::new(instance, ttl, cache_flush, RData::Txt(a.txt()));
    let addrs: Vec<Record> = v4
        .iter()
        .map(|ip| Record::new(host.clone(), ttl, cache_flush, RData::A(*ip)))
        .chain(v6.iter().map(|ip| Record::new(host.clone(), ttl, cache_flush, RData::Aaaa(*ip))))
        .collect();
    let mut answers = Vec::new();
    let mut additionals = Vec::new();
    if answer.types {
        answers.push(Record::new(Name::parse(SERVICE_TYPES), ttl, false, RData::Ptr(service)));
    }
    if answer.ptr {
        answers.push(ptr);
    }
    for (wanted, rec) in [(answer.srv, srv), (answer.txt, txt)] {
        if wanted {
            answers.push(rec);
        } else if answer.ptr {
            additionals.push(rec);
        }
    }
    if answer.addr {
        answers.extend(addrs);
    } else if answer.ptr || answer.srv {
        additionals.extend(addrs);
    }
    Message {
        id: 0,
        flags: dns::FLAG_RESPONSE | dns::FLAG_AUTHORITATIVE,
        questions: Vec::new(),
        answers,
        authorities: Vec::new(),
        additionals,
    }
}

/// What a query asks of `a` (after known-answer suppression, RFC 6762 §7.1), and whether any of
/// those questions wants a unicast reply.
fn wanted(a: &Advert, q: &Message) -> (Kinds, bool) {
    let service = service();
    let types = Name::parse(SERVICE_TYPES);
    let instance = a.instance_name();
    let host = a.host_name();
    let mut k = Kinds::default();
    let mut unicast = false;
    for question in &q.questions {
        let t = question.qtype;
        let any = t == dns::TYPE_ANY;
        let before = k;
        if question.name == service && (t == dns::TYPE_PTR || any) {
            k.ptr = true;
        }
        if question.name == types && (t == dns::TYPE_PTR || any) {
            k.types = true;
        }
        if question.name == instance {
            k.srv |= t == dns::TYPE_SRV || any;
            k.txt |= t == dns::TYPE_TXT || any;
        }
        if question.name == host && (t == dns::TYPE_A || t == dns::TYPE_AAAA || any) {
            k.addr = true;
        }
        if k != before && question.unicast {
            unicast = true;
        }
    }
    let known = |owner: &Name, target: &Name| {
        q.answers
            .iter()
            .any(|r| r.name == *owner && r.ttl >= TTL / 2 && matches!(&r.data, RData::Ptr(n) if n == target))
    };
    if k.ptr && known(&service, &instance) {
        k.ptr = false;
    }
    if k.types && known(&types, &service) {
        k.types = false;
    }
    (k, unicast)
}

/// The reply to a legacy (one-shot, non-5353 port) query: unicast, the query's id and question
/// repeated, short TTLs, no cache-flush bits (RFC 6762 §6.7).
fn legacy_reply(a: &Advert, q: &Message, k: Kinds, v4: &[Ipv4Addr], v6: &[Ipv6Addr]) -> Message {
    let mut m = reply(a, k, LEGACY_TTL, false, v4, v6);
    m.id = q.id;
    m.questions = q.questions.clone();
    m
}

// ------------------------------------------------------------------------------ who we heard

/// A kiln instance seen on the network.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Instance {
    pub id: String,
    /// Its DNS-SD instance name.
    pub instance: String,
    /// Its host name.
    pub name: String,
    /// `server` or `agent`.
    pub role: String,
    pub version: String,
    pub os: String,
    pub arch: String,
    /// HTTP port (0: none).
    pub port: u16,
    /// QUIC port (0: none).
    pub quic: u16,
    pub ca: Option<String>,
    pub enrolled: Option<String>,
    /// The address its packets came from first, then its A/AAAA records.
    pub addresses: Vec<IpAddr>,
    pub first_seen: i64,
    pub last_seen: i64,
    #[serde(skip)]
    pub expires: Instant,
}

impl Instance {
    /// The address to reach it at: the first IPv4 one, else the first.
    pub fn address(&self) -> Option<IpAddr> {
        self.addresses.iter().find(|a| a.is_ipv4()).or_else(|| self.addresses.first()).copied()
    }

    /// Its HTTP URL, when it serves HTTP.
    pub fn url(&self) -> Option<String> {
        if self.port == 0 {
            return None;
        }
        Some(match self.address()? {
            IpAddr::V4(ip) => format!("http://{ip}:{}", self.port),
            IpAddr::V6(ip) => format!("http://[{ip}]:{}", self.port),
        })
    }

    /// Everything but the bookkeeping times.
    fn same(&self, o: &Self) -> bool {
        (&self.id, &self.instance, &self.name, &self.role, &self.version, &self.os, &self.arch)
            == (&o.id, &o.instance, &o.name, &o.role, &o.version, &o.os, &o.arch)
            && (self.port, self.quic, &self.ca, &self.enrolled, &self.addresses) == (o.port, o.quic, &o.ca, &o.enrolled, &o.addresses)
    }
}

/// Printable text from the network, at most `max` characters.
fn clean(s: Option<&str>, max: usize) -> String {
    s.unwrap_or_default().chars().filter(|c| !c.is_control()).take(max).collect()
}

fn hex16(s: &str) -> Option<String> {
    (s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit())).then(|| s.to_ascii_lowercase())
}

/// What a response tells about kiln instances: those it describes (complete with SRV and TXT,
/// TTL > 0) and the instance names it says goodbye to (TTL 0).
fn heard(msg: &Message, from: SocketAddr, now: Instant) -> (Vec<Instance>, Vec<String>) {
    let service = service();
    let mut names: Vec<Name> = Vec::new();
    let mut gone: Vec<String> = Vec::new();
    for r in msg.records() {
        let named = match &r.data {
            RData::Ptr(target) if r.name == service && target.parent() == service => Some(target),
            RData::Srv { .. } | RData::Txt(_) if r.name.parent() == service => Some(&r.name),
            _ => None,
        };
        let Some(name) = named else { continue };
        if r.ttl == 0 {
            let label = name.first().unwrap_or_default().to_owned();
            if !gone.contains(&label) {
                gone.push(label);
            }
        } else if !names.contains(name) && names.len() < MAX_PER_PACKET {
            names.push(name.clone());
        }
    }
    let unix = crate::util::now_unix();
    let mut seen = Vec::new();
    for name in names {
        let label = name.first().unwrap_or_default().to_owned();
        if gone.contains(&label) {
            continue;
        }
        let srv = msg.records().find_map(|r| match &r.data {
            RData::Srv { port, target, .. } if r.name == name => Some((*port, target.clone(), r.ttl)),
            _ => None,
        });
        let txt = msg.records().find_map(|r| match &r.data {
            RData::Txt(strings) if r.name == name => Some((dns::txt_pairs(strings), r.ttl)),
            _ => None,
        });
        let (Some((port, target, srv_ttl)), Some((pairs, txt_ttl))) = (srv, txt) else { continue };
        let get = |k: &str| pairs.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
        let id: String = get("id").unwrap_or_default().chars().filter(char::is_ascii_alphanumeric).take(64).collect();
        if id.is_empty() {
            continue;
        }
        let mut addresses: Vec<IpAddr> = Vec::new();
        if !from.ip().is_unspecified() {
            addresses.push(from.ip());
        }
        for r in msg.records().filter(|r| r.name == target && r.ttl > 0) {
            let ip = match r.data {
                RData::A(ip) => IpAddr::V4(ip),
                RData::Aaaa(ip) => IpAddr::V6(ip),
                _ => continue,
            };
            if !addresses.contains(&ip) && addresses.len() < 16 {
                addresses.push(ip);
            }
        }
        let ttl = srv_ttl.min(txt_ttl).clamp(1, 4500);
        seen.push(Instance {
            id,
            instance: clean(Some(&label), 63),
            name: clean(get("name"), 64),
            role: clean(get("role"), 16),
            version: clean(get("version"), 32),
            os: clean(get("os"), 32),
            arch: clean(get("arch"), 32),
            port,
            quic: get("quic").and_then(|q| q.parse().ok()).unwrap_or(0),
            ca: get("ca").and_then(hex16),
            enrolled: get("enrolled").and_then(hex16),
            addresses,
            first_seen: unix,
            last_seen: unix,
            expires: now + Duration::from_secs(u64::from(ttl)),
        });
    }
    (seen, gone)
}

/// The kiln instances heard on the network, by instance id.
#[derive(Default)]
pub struct Table {
    inner: Mutex<HashMap<String, Instance>>,
}

impl Table {
    /// Every instance, servers first, then by name.
    pub fn snapshot(&self) -> Vec<Instance> {
        let mut v: Vec<Instance> = self.inner.lock().values().cloned().collect();
        v.sort_by(|a, b| (a.role != "server", &a.name, &a.id).cmp(&(b.role != "server", &b.name, &b.id)));
        v
    }

    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().is_empty()
    }

    /// Record a sighting; true when the instance is new or something about it changed.
    fn upsert(&self, mut seen: Instance) -> bool {
        let mut g = self.inner.lock();
        if let Some(have) = g.get_mut(&seen.id) {
            let changed = !have.same(&seen);
            seen.first_seen = have.first_seen;
            *have = seen;
            return changed;
        }
        // The same instance name under a new id (a reinstall): the old entry is stale.
        g.retain(|_, i| i.instance != seen.instance);
        if g.len() >= MAX_INSTANCES
            && let Some(oldest) = g.values().min_by_key(|i| i.expires).map(|i| i.id.clone())
        {
            g.remove(&oldest);
        }
        g.insert(seen.id.clone(), seen);
        true
    }

    fn forget_instance(&self, label: &str) -> bool {
        let mut g = self.inner.lock();
        let before = g.len();
        g.retain(|_, i| i.instance != label);
        g.len() != before
    }

    fn sweep(&self, now: Instant) -> bool {
        let mut g = self.inner.lock();
        let before = g.len();
        g.retain(|_, i| i.expires > now);
        g.len() != before
    }

    /// Instance names with the seconds their records have left (known answers for a query).
    fn known(&self, now: Instant) -> Vec<(String, u32)> {
        self.inner
            .lock()
            .values()
            .map(|i| (i.instance.clone(), u32::try_from(i.expires.saturating_duration_since(now).as_secs()).unwrap_or(0)))
            .collect()
    }
}

// ------------------------------------------------------------------------------ interfaces

/// A network interface kiln uses for multicast DNS.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Iface {
    pub name: String,
    pub index: u32,
    /// IPv4 addresses with their netmasks.
    pub v4: Vec<(Ipv4Addr, Ipv4Addr)>,
    pub v6: Vec<Ipv6Addr>,
}

/// Container and VM plumbing rather than a LAN.
fn is_virtual(name: &str) -> bool {
    const PLUMBING: [&str; 14] = [
        "docker", "veth", "br-", "virbr", "vnet", "cni", "flannel", "cali", "cilium", "vxlan", "kube", "lxcbr", "lxdbr", "podman",
    ];
    PLUMBING.iter().any(|p| name.starts_with(p))
}

/// The interfaces to use: `allow` when given, else every interface that is up, running and
/// multicast-capable, except loopback, point-to-point links (VPNs) and container/VM bridges.
pub fn interfaces(allow: &[String]) -> Vec<Iface> {
    use nix::net::if_::InterfaceFlags as F;
    let Ok(addrs) = nix::ifaddrs::getifaddrs() else { return Vec::new() };
    let mut out: Vec<Iface> = Vec::new();
    for ifa in addrs {
        let f = ifa.flags;
        if allow.is_empty() {
            let usable = f.contains(F::IFF_UP) && f.contains(F::IFF_RUNNING) && f.contains(F::IFF_MULTICAST);
            if !usable || f.contains(F::IFF_LOOPBACK) || f.contains(F::IFF_POINTOPOINT) || is_virtual(&ifa.interface_name) {
                continue;
            }
        } else if !allow.contains(&ifa.interface_name) {
            continue;
        }
        let Some(addr) = ifa.address.as_ref() else { continue };
        let at = match out.iter().position(|i| i.name == ifa.interface_name) {
            Some(at) => at,
            None => {
                let index = nix::net::if_::if_nametoindex(ifa.interface_name.as_str()).unwrap_or(0);
                out.push(Iface { name: ifa.interface_name.clone(), index, v4: Vec::new(), v6: Vec::new() });
                out.len() - 1
            }
        };
        let Some(entry) = out.get_mut(at) else { continue };
        if let Some(sin) = addr.as_sockaddr_in() {
            let mask = ifa.netmask.as_ref().and_then(|m| m.as_sockaddr_in()).map_or(Ipv4Addr::BROADCAST, |m| m.ip());
            entry.v4.push((sin.ip(), mask));
        } else if let Some(sin6) = addr.as_sockaddr_in6() {
            entry.v6.push(sin6.ip());
        }
    }
    out.retain(|i| i.index != 0 && (!i.v4.is_empty() || !i.v6.is_empty()));
    out
}

fn same_subnet(addr: Ipv4Addr, mask: Ipv4Addr, other: Ipv4Addr) -> bool {
    u32::from(addr) & u32::from(mask) == u32::from(other) & u32::from(mask)
}

/// This machine's address on the route to `remote` (no packet is sent).
pub fn local_address_for(remote: IpAddr) -> Option<IpAddr> {
    let bind: SocketAddr = if remote.is_ipv4() { (Ipv4Addr::UNSPECIFIED, 0).into() } else { (Ipv6Addr::UNSPECIFIED, 0).into() };
    let s = std::net::UdpSocket::bind(bind).ok()?;
    s.connect((remote, 9)).ok()?;
    s.local_addr().ok().map(|a| a.ip()).filter(|ip| !ip.is_unspecified())
}

// ------------------------------------------------------------------------------ the engine

/// How a discovery engine runs.
#[derive(Clone)]
pub struct Options {
    /// What to announce; `None` only browses.
    pub advert: Option<Arc<Mutex<Advert>>>,
    pub browse: bool,
    /// Interface allow-list (empty: automatic, see [`interfaces`]).
    pub interfaces: Vec<String>,
    /// Announce IPv6 addresses too (when the HTTP server listens on IPv6).
    pub ipv6_addresses: bool,
    /// The local UDP port: 5353 for a full responder and browser; another (0: any) for a querier
    /// whose answers come back by unicast, and for tests.
    pub port: u16,
    /// Join the groups and send to them (off in tests, which run over loopback unicast).
    pub multicast: bool,
    /// Notified when the advert changed, to announce it at once.
    pub changed: Arc<Notify>,
}

impl Options {
    pub fn browse_only() -> Self {
        Self {
            advert: None,
            browse: true,
            interfaces: Vec::new(),
            ipv6_addresses: false,
            port: MDNS_PORT,
            multicast: true,
            changed: Arc::new(Notify::new()),
        }
    }
}

/// A UDP socket adopted by hyper-rt, plus a duplicate handle of it for socket options.
struct Sock {
    io: hyper_rt::udp::UdpSocket,
    ctl: socket2::Socket,
}

fn open_socket(v6: bool, port: u16) -> anyhow::Result<Sock> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(if v6 { Domain::IPV6 } else { Domain::IPV4 }, Type::DGRAM, Some(Protocol::UDP)).context("socket")?;
    s.set_reuse_address(true).context("SO_REUSEADDR")?;
    #[cfg(unix)]
    s.set_reuse_port(true).context("SO_REUSEPORT")?;
    let bind: SocketAddr = if v6 {
        s.set_only_v6(true).context("IPV6_V6ONLY")?;
        let _ = s.set_multicast_loop_v6(true);
        let _ = s.set_multicast_hops_v6(255);
        (Ipv6Addr::UNSPECIFIED, port).into()
    } else {
        let _ = s.set_multicast_loop_v4(true);
        let _ = s.set_multicast_ttl_v4(255);
        (Ipv4Addr::UNSPECIFIED, port).into()
    };
    s.bind(&bind.into()).with_context(|| format!("bind UDP {bind}"))?;
    let ctl = s.try_clone().context("duplicate the socket")?;
    let std_socket: std::net::UdpSocket = s.into();
    let io = hyper_rt::udp::UdpSocket::adopt(std_socket.into()).map_err(|e| anyhow::anyhow!("hand the socket to hyper-rt: {e}"))?;
    Ok(Sock { io, ctl })
}

/// A multicast reply waiting for its random delay.
struct Pending {
    due: Instant,
    kinds: Kinds,
    /// The interface the query came in on (`None`: every interface).
    iface: Option<u32>,
}

/// One mDNS responder and browser: its sockets, the interfaces it joined, its schedule.
pub struct Engine {
    opts: Options,
    v4: Sock,
    v6: Option<Sock>,
    ifaces: Vec<Iface>,
    joined4: HashSet<u32>,
    joined6: HashSet<u32>,
    /// When our records were last multicast on each interface (at most once a second).
    last_sent: HashMap<u32, Instant>,
    pending: Option<Pending>,
    announced: bool,
    table: Arc<Table>,
}

impl Engine {
    /// Bind the sockets (IPv6 is optional) and join the multicast groups.
    pub fn open(opts: Options, table: Arc<Table>) -> anyhow::Result<Self> {
        let v4 = open_socket(false, opts.port)?;
        let v6 = if opts.multicast && opts.port == MDNS_PORT {
            open_socket(true, opts.port).map_err(|e| tracing::debug!(error = %format!("{e:#}"), "no IPv6 multicast DNS")).ok()
        } else {
            None
        };
        let mut e = Self {
            opts,
            v4,
            v6,
            ifaces: Vec::new(),
            joined4: HashSet::new(),
            joined6: HashSet::new(),
            last_sent: HashMap::new(),
            pending: None,
            announced: false,
            table,
        };
        e.refresh_interfaces();
        Ok(e)
    }

    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.v4.io.local_addr().ok()
    }

    pub fn interface_names(&self) -> Vec<String> {
        self.ifaces.iter().map(|i| i.name.clone()).collect()
    }

    fn advert(&self) -> Option<Advert> {
        self.opts.advert.as_ref().map(|a| a.lock().clone())
    }

    /// Re-read the interfaces and join the groups on new ones. True when they changed.
    fn refresh_interfaces(&mut self) -> bool {
        if !self.opts.multicast {
            return false;
        }
        let now = interfaces(&self.opts.interfaces);
        if self.opts.port == MDNS_PORT {
            for i in &now {
                if let Some((addr, _)) = i.v4.first()
                    && !self.joined4.contains(&i.index)
                {
                    match self.v4.ctl.join_multicast_v4(&GROUP_V4, addr) {
                        Ok(()) => {
                            self.joined4.insert(i.index);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                            self.joined4.insert(i.index);
                        }
                        Err(e) => tracing::debug!(iface = %i.name, error = %e, "cannot join 224.0.0.251"),
                    }
                }
                if let Some(v6) = &self.v6
                    && !i.v6.is_empty()
                    && !self.joined6.contains(&i.index)
                {
                    match v6.ctl.join_multicast_v6(&GROUP_V6, i.index) {
                        Ok(()) => {
                            self.joined6.insert(i.index);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                            self.joined6.insert(i.index);
                        }
                        Err(e) => tracing::debug!(iface = %i.name, error = %e, "cannot join ff02::fb"),
                    }
                }
            }
        }
        let changed = now != self.ifaces;
        if changed {
            tracing::debug!(interfaces = ?now.iter().map(|i| &i.name).collect::<Vec<_>>(), "multicast DNS interfaces");
        }
        self.ifaces = now;
        changed
    }

    /// Our addresses on `iface` (IPv6 only when announced).
    fn addrs(&self, iface: &Iface) -> (Vec<Ipv4Addr>, Vec<Ipv6Addr>) {
        let v6 = if self.opts.ipv6_addresses { iface.v6.clone() } else { Vec::new() };
        (iface.v4.iter().map(|(a, _)| *a).collect(), v6)
    }

    fn all_addrs(&self) -> (Vec<Ipv4Addr>, Vec<Ipv6Addr>) {
        let mut v4 = Vec::new();
        let mut v6 = Vec::new();
        for i in &self.ifaces {
            let (a, b) = self.addrs(i);
            v4.extend(a);
            v6.extend(b);
        }
        (v4, v6)
    }

    /// The interface a packet from `from` arrived on, by subnet (IPv4) or scope (IPv6).
    fn iface_for(&self, from: SocketAddr) -> Option<&Iface> {
        match from {
            SocketAddr::V4(a) => self.ifaces.iter().find(|i| i.v4.iter().any(|(addr, mask)| same_subnet(*addr, *mask, *a.ip()))),
            SocketAddr::V6(a) => self.ifaces.iter().find(|i| a.scope_id() != 0 && i.index == a.scope_id()),
        }
    }

    fn send(&self, msg: &Message, to: SocketAddr, via: Option<&Iface>) {
        let bytes = match msg.encode() {
            Ok(b) => b,
            Err(e) => {
                tracing::debug!(error = %e, "cannot encode a multicast DNS message");
                return;
            }
        };
        let (sock, set) = match to {
            SocketAddr::V4(_) => {
                let set = via.and_then(|i| i.v4.first()).map(|(addr, _)| self.v4.ctl.set_multicast_if_v4(addr));
                (&self.v4, set)
            }
            SocketAddr::V6(_) => {
                let Some(s) = &self.v6 else { return };
                (s, via.map(|i| s.ctl.set_multicast_if_v6(i.index)))
            }
        };
        if let Some(Err(e)) = set {
            tracing::debug!(error = %e, "cannot pick the outgoing interface");
            return;
        }
        match sock.io.try_send_to(&bytes, to) {
            Ok(Some(_)) => {}
            // mDNS is best effort: the next announcement or query covers a dropped datagram.
            Ok(None) => tracing::debug!(%to, "send buffer full; multicast DNS datagram dropped"),
            Err(e) => tracing::debug!(%to, error = %e, "multicast DNS send failed"),
        }
    }

    /// Send `msg(addresses)` to the groups on each interface (or the default one when none is
    /// known); `only` limits it to one interface.
    fn multicast(&mut self, only: Option<u32>, now: Instant, rate_limit: bool, build: impl Fn(&[Ipv4Addr], &[Ipv6Addr]) -> Message) {
        if !self.opts.multicast {
            return;
        }
        let group4: SocketAddr = (GROUP_V4, MDNS_PORT).into();
        if self.ifaces.is_empty() {
            let (v4, v6) = self.all_addrs();
            self.send(&build(&v4, &v6), group4, None);
            return;
        }
        let ifaces: Vec<Iface> = self.ifaces.iter().filter(|i| only.is_none_or(|x| x == i.index)).cloned().collect();
        for i in &ifaces {
            if rate_limit && self.last_sent.get(&i.index).is_some_and(|t| now.saturating_duration_since(*t) < Duration::from_secs(1)) {
                continue;
            }
            let (v4, v6) = self.addrs(i);
            let msg = build(&v4, &v6);
            if !i.v4.is_empty() {
                self.send(&msg, group4, Some(i));
            }
            if self.v6.is_some() && !i.v6.is_empty() {
                self.send(&msg, SocketAddr::V6(SocketAddrV6::new(GROUP_V6, MDNS_PORT, 0, i.index)), Some(i));
            }
            self.last_sent.insert(i.index, now);
        }
    }

    /// Announce our records (`ttl` 0: say goodbye).
    fn announce(&mut self, ttl: u32, now: Instant) {
        let Some(advert) = self.advert() else { return };
        self.multicast(None, now, false, |v4, v6| reply(&advert, Kinds::ALL, ttl, true, v4, v6));
        self.announced = ttl > 0;
    }

    /// Ask for `_kiln._tcp.local.`, listing the instances already known.
    fn query(&mut self, now: Instant) {
        let service = service();
        let known: Vec<Record> = self
            .table
            .known(now)
            .into_iter()
            .filter(|(_, ttl)| *ttl > TTL / 2)
            .take(24)
            .map(|(label, ttl)| Record::new(service.clone(), ttl, false, RData::Ptr(Name::child(&label, &service))))
            .collect();
        let msg = Message {
            questions: vec![Question { name: service.clone(), qtype: dns::TYPE_PTR, qclass: dns::CLASS_IN, unicast: false }],
            answers: known,
            ..Message::default()
        };
        self.multicast(None, now, false, |_, _| msg.clone());
    }

    /// Handle one datagram. True when the table changed.
    fn on_packet(&mut self, data: &[u8], from: SocketAddr, now: Instant) -> bool {
        let Ok(msg) = Message::decode(data) else {
            tracing::trace!(%from, "undecodable multicast DNS packet");
            return false;
        };
        if msg.is_response() {
            if !self.opts.browse {
                return false;
            }
            let me = self.advert().map(|a| a.id);
            let (seen, gone) = heard(&msg, from, now);
            let mut changed = false;
            for i in seen {
                if me.as_deref() != Some(i.id.as_str()) {
                    changed |= self.table.upsert(i);
                }
            }
            for label in gone {
                changed |= self.table.forget_instance(&label);
            }
            return changed;
        }
        // A standard query (opcode 0) about us?
        let Some(advert) = self.advert() else { return false };
        if msg.flags & 0x7800 != 0 {
            return false;
        }
        let (kinds, unicast) = wanted(&advert, &msg);
        if !kinds.any() {
            return false;
        }
        let iface = self.iface_for(from).cloned();
        let (v4, v6) = iface.as_ref().map_or_else(|| self.all_addrs(), |i| self.addrs(i));
        if from.port() != MDNS_PORT {
            self.send(&legacy_reply(&advert, &msg, kinds, &v4, &v6), from, None);
        } else if unicast {
            self.send(&reply(&advert, kinds, TTL, true, &v4, &v6), from, None);
        } else {
            let due = now + Duration::from_millis(20 + rand::random::<u64>() % 100);
            let index = iface.map(|i| i.index);
            self.pending = Some(match self.pending.take() {
                Some(p) => Pending { due: p.due.min(due), kinds: p.kinds.union(kinds), iface: if p.iface == index { index } else { None } },
                None => Pending { due, kinds, iface: index },
            });
        }
        false
    }

    fn flush_pending(&mut self, now: Instant) {
        let Some(p) = self.pending.take() else { return };
        let Some(advert) = self.advert() else { return };
        self.multicast(p.iface, now, true, |v4, v6| reply(&advert, p.kinds, TTL, true, v4, v6));
    }

    /// Serve until `shutdown`: answer, announce, browse. Dropping the engine says goodbye.
    pub async fn serve(mut self, shutdown: Shutdown, on_change: &(dyn Fn() + Send + Sync)) -> anyhow::Result<()> {
        enum Ev {
            Stop,
            Changed,
            Packet(Result<(usize, SocketAddr), hyper_rt::RtError>, bool),
            Tick,
        }
        let start = Instant::now();
        let mut next_announce = self.opts.advert.is_some().then_some(start);
        let mut announcements = 0u32;
        let mut next_query = self.opts.browse.then_some(start);
        let mut queries = 0u32;
        let mut next_sweep = start + SWEEP_EVERY;
        let mut next_ifaces = start + INTERFACES_EVERY;
        let mut buf4 = vec![0u8; dns::MAX_MESSAGE];
        let mut buf6 = vec![0u8; dns::MAX_MESSAGE];
        let mut errors = 0u32;
        let changed = self.opts.changed.clone();
        loop {
            let deadline = [next_announce, next_query, Some(next_sweep), Some(next_ifaces), self.pending.as_ref().map(|p| p.due)]
                .into_iter()
                .flatten()
                .min()
                .unwrap_or(next_sweep);
            let ev = {
                let v6 = self.v6.as_ref().map(|s| &s.io);
                let r4 = self.v4.io.recv_from(&mut buf4).fuse();
                let r6 = async {
                    match v6 {
                        Some(s) => s.recv_from(&mut buf6).await,
                        None => futures::future::pending().await,
                    }
                }
                .fuse();
                let timer = crate::rt::sleep_until(deadline).fuse();
                let stop = shutdown.cancelled().fuse();
                let notified = changed.notified().fuse();
                futures::pin_mut!(r4, r6, timer, stop, notified);
                futures::select_biased! {
                    () = stop => Ev::Stop,
                    r = r4 => Ev::Packet(r, false),
                    r = r6 => Ev::Packet(r, true),
                    () = notified => Ev::Changed,
                    () = timer => Ev::Tick,
                }
            };
            let now = Instant::now();
            match ev {
                Ev::Stop => return Ok(()),
                Ev::Changed => {
                    next_announce = self.opts.advert.is_some().then_some(now);
                    announcements = 0;
                }
                Ev::Packet(Ok((n, from)), v6) => {
                    errors = 0;
                    let data = if v6 { buf6.get(..n) } else { buf4.get(..n) }.unwrap_or_default();
                    if self.on_packet(data, from, now) {
                        on_change();
                    }
                }
                Ev::Packet(Err(e), _) => {
                    errors += 1;
                    if errors > 50 {
                        anyhow::bail!("multicast DNS socket keeps failing: {e}");
                    }
                    tracing::debug!(error = %e, "multicast DNS receive failed");
                    crate::rt::sleep(Duration::from_millis(20)).await;
                }
                Ev::Tick => {}
            }
            if next_announce.is_some_and(|t| t <= now) {
                self.announce(TTL, now);
                announcements += 1;
                // Twice more, a second apart, then periodically (RFC 6762 §8.3).
                next_announce = Some(now + if announcements < 3 { Duration::from_secs(1) } else { ANNOUNCE_EVERY });
            }
            if next_query.is_some_and(|t| t <= now) {
                self.query(now);
                queries += 1;
                // At 0, 1 and 3 s; after that the others' announcements keep the table fresh.
                next_query = match queries {
                    1 => Some(now + Duration::from_secs(1)),
                    2 => Some(now + Duration::from_secs(2)),
                    _ => None,
                };
            }
            if self.pending.as_ref().is_some_and(|p| p.due <= now) {
                self.flush_pending(now);
            }
            if next_sweep <= now {
                if self.table.sweep(now) {
                    on_change();
                }
                next_sweep = now + SWEEP_EVERY;
            }
            if next_ifaces <= now {
                if self.refresh_interfaces() && self.opts.advert.is_some() {
                    next_announce = Some(now);
                }
                next_ifaces = now + INTERFACES_EVERY;
            }
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        if self.announced {
            self.announce(0, Instant::now());
        }
    }
}

/// Run discovery until `shutdown`. Without a usable port 5353 or multicast network kiln works on
/// without discovery and tries again every minute; a failure while serving is returned (the
/// supervisor restarts it).
pub async fn run(opts: Options, table: Arc<Table>, shutdown: Shutdown, on_change: Arc<dyn Fn() + Send + Sync>) -> anyhow::Result<()> {
    let mut warned = false;
    loop {
        match Engine::open(opts.clone(), Arc::clone(&table)) {
            Ok(engine) => {
                tracing::info!(
                    service = SERVICE,
                    interfaces = ?engine.interface_names(),
                    announce = opts.advert.is_some(),
                    browse = opts.browse,
                    "discovery: multicast DNS running"
                );
                return engine.serve(shutdown, &*on_change).await;
            }
            Err(e) => {
                if warned {
                    tracing::debug!(error = %format!("{e:#}"), "discovery still unavailable");
                } else {
                    tracing::warn!(error = %format!("{e:#}"), "discovery unavailable (multicast DNS on UDP 5353); retrying every minute");
                    warned = true;
                }
                if !shutdown.sleep(Duration::from_secs(60)).await {
                    return Ok(());
                }
            }
        }
    }
}

/// Look for kiln instances for `wait`: a browse-only engine sharing port 5353, or, when that port
/// cannot be shared, one-shot queries from another port whose answers come back by unicast.
pub async fn browse_once(wait: Duration, interfaces: &[String]) -> anyhow::Result<Vec<Instance>> {
    let table = Arc::new(Table::default());
    let mut opts = Options { interfaces: interfaces.to_vec(), ..Options::browse_only() };
    let engine = match Engine::open(opts.clone(), Arc::clone(&table)) {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!(error = %format!("{e:#}"), "port 5353 unavailable; querying from another port");
            opts.port = 0;
            Engine::open(opts, Arc::clone(&table)).context("open a multicast DNS socket")?
        }
    };
    let stop = Shutdown::new();
    let timer = {
        let stop = stop.clone();
        async move {
            crate::rt::sleep(wait).await;
            stop.cancel();
        }
    };
    let (served, ()) = futures::join!(engine.serve(stop, &|| {}), timer);
    served?;
    Ok(table.snapshot())
}

// ------------------------------------------------------------------------------ in the server

/// The server's discovery state, shared with the API.
pub struct Discovery {
    pub settings: crate::config::Discovery,
    pub advert: Arc<Mutex<Advert>>,
    pub table: Arc<Table>,
    pub changed: Arc<Notify>,
    /// The HTTP listener accepts IPv6, so AAAA records are worth announcing.
    ipv6: bool,
}

fn port_of(addr: &str) -> Option<u16> {
    addr.rsplit_once(':').and_then(|(_, p)| p.trim_end_matches('/').parse().ok())
}

impl Discovery {
    pub fn for_server(cfg: &crate::config::Config, ca_der: &[u8]) -> Self {
        let http = port_of(&cfg.server.http_addr).unwrap_or(7070);
        let quic = port_of(&cfg.server.quic_advertise).or_else(|| port_of(&cfg.server.quic_addr)).unwrap_or(7071);
        let mut advert = Advert::new(instance_id(&cfg.server.data_dir), Role::Server, http, quic);
        advert.ca = Some(ca_fingerprint(ca_der));
        Self {
            settings: cfg.discovery.clone(),
            advert: Arc::new(Mutex::new(advert)),
            table: Arc::new(Table::default()),
            changed: Arc::new(Notify::new()),
            ipv6: cfg.server.http_addr.starts_with('['),
        }
    }

    pub fn options(&self) -> Options {
        Options {
            advert: self.settings.announce.then(|| Arc::clone(&self.advert)),
            browse: self.settings.browse,
            interfaces: self.settings.interfaces.clone(),
            ipv6_addresses: self.ipv6,
            port: MDNS_PORT,
            multicast: true,
            changed: Arc::clone(&self.changed),
        }
    }

    /// The server's discovery child: announce and browse, telling UI clients when the table changes.
    pub async fn serve(hub: Arc<crate::hub::Hub>) -> anyhow::Result<()> {
        let weak = Arc::downgrade(&hub);
        let on_change: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            if let Some(h) = weak.upgrade() {
                h.emit(&serde_json::json!({ "type": "discovery" }));
            }
        });
        let d = &hub.discovery;
        run(d.options(), Arc::clone(&d.table), hub.shutdown.clone(), on_change).await
    }
}

/// An agent started with `--discover`: announced as role `agent` while it runs.
pub struct AgentAnnouncer {
    pub advert: Arc<Mutex<Advert>>,
    pub table: Arc<Table>,
    changed: Arc<Notify>,
    task: crate::rt::JoinHandle<()>,
}

impl AgentAnnouncer {
    pub fn start(state_dir: &Path, enrolled_ca_der: Option<&[u8]>, interfaces: Vec<String>, shutdown: Shutdown) -> Self {
        let mut advert = Advert::new(instance_id(state_dir), Role::Agent, 0, 0);
        advert.enrolled = enrolled_ca_der.map(ca_fingerprint);
        let advert = Arc::new(Mutex::new(advert));
        let table = Arc::new(Table::default());
        let changed = Arc::new(Notify::new());
        let opts = Options { advert: Some(Arc::clone(&advert)), interfaces, changed: Arc::clone(&changed), ..Options::browse_only() };
        let t = Arc::clone(&table);
        let task = crate::rt::spawn(async move {
            if let Err(e) = run(opts, t, shutdown, Arc::new(|| {})).await {
                tracing::warn!(error = %format!("{e:#}"), "discovery stopped");
            }
        });
        Self { advert, table, changed, task }
    }

    /// The agent enrolled: say so in its announcements.
    pub fn enrolled(&self, ca_der: &[u8]) {
        self.advert.lock().enrolled = Some(ca_fingerprint(ca_der));
        self.changed.notify();
    }

    /// Wait (briefly) for the goodbye to go out after shutdown.
    pub async fn finish(self) {
        let _ = crate::rt::timeout(Duration::from_millis(500), self.task).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn advert() -> Advert {
        let mut a = Advert::new("1a2b3c4d5e6f70819293a4b5c6d7e8f9".into(), Role::Server, 7070, 7071);
        a.name = "box".into();
        a.ca = Some("0123456789abcdef".into());
        a
    }

    fn from() -> SocketAddr {
        "192.168.1.80:5353".parse().unwrap_or_else(|_| SocketAddr::from((Ipv4Addr::LOCALHOST, 5353)))
    }

    #[test]
    fn announcements_are_heard_back() -> anyhow::Result<()> {
        let a = advert();
        let msg = reply(&a, Kinds::ALL, TTL, true, &[Ipv4Addr::new(192, 168, 1, 80), Ipv4Addr::new(10, 0, 0, 2)], &[]);
        let wire = Message::decode(&msg.encode()?)?;
        let now = Instant::now();
        let (seen, gone) = heard(&wire, from(), now);
        assert!(gone.is_empty());
        let i = seen.first().ok_or_else(|| anyhow::anyhow!("nothing heard"))?;
        assert_eq!(seen.len(), 1);
        assert_eq!(i.id, a.id);
        assert_eq!(i.instance, "kiln server on box (1a2b3c4d)");
        assert_eq!((i.name.as_str(), i.role.as_str(), i.port, i.quic), ("box", "server", 7070, 7071));
        assert_eq!(i.ca.as_deref(), Some("0123456789abcdef"));
        assert_eq!(i.enrolled, None);
        // The source address first (deduplicated), then the other A record.
        assert_eq!(i.addresses, vec!["192.168.1.80".parse::<IpAddr>()?, "10.0.0.2".parse::<IpAddr>()?]);
        assert_eq!(i.url().as_deref(), Some("http://192.168.1.80:7070"));
        assert!(i.expires > now + Duration::from_secs(100));

        // A goodbye names the instance and describes nothing.
        let bye = reply(&a, Kinds::ALL, 0, true, &[], &[]);
        let (seen, gone) = heard(&Message::decode(&bye.encode()?)?, from(), now);
        assert!(seen.is_empty());
        assert_eq!(gone, vec![a.instance_label()]);

        // An agent without HTTP has no URL; its enrollment is announced.
        let mut ag = Advert::new("ffeeddccbbaa99887766554433221100".into(), Role::Agent, 0, 0);
        ag.enrolled = Some("0123456789ABCDEF".into());
        let (seen, _) = heard(&reply(&ag, Kinds::ALL, TTL, true, &[], &[]), from(), now);
        let i = seen.first().ok_or_else(|| anyhow::anyhow!("agent not heard"))?;
        assert_eq!(i.role, "agent");
        assert_eq!(i.url(), None);
        assert_eq!(i.enrolled.as_deref(), Some("0123456789abcdef"));
        Ok(())
    }

    #[test]
    fn incomplete_or_foreign_responses_are_ignored() -> anyhow::Result<()> {
        let a = advert();
        // PTR alone (no SRV/TXT) describes nothing.
        let only_ptr = reply(&a, Kinds { ptr: true, ..Kinds::default() }, TTL, true, &[], &[]);
        let mut only_ptr = only_ptr;
        only_ptr.additionals.clear();
        assert!(heard(&only_ptr, from(), Instant::now()).0.is_empty());
        // Another service type is not ours.
        let other = Message {
            answers: vec![Record::new(Name::parse("_http._tcp.local"), TTL, false, RData::Ptr(Name::parse("web._http._tcp.local")))],
            ..Message::default()
        };
        assert!(heard(&other, from(), Instant::now()).0.is_empty());
        // A TXT without an id is skipped; hostile values are cleaned.
        let instance = a.instance_name();
        let msg = Message {
            answers: vec![
                Record::new(instance.clone(), TTL, true, RData::Srv { priority: 0, weight: 0, port: 1, target: Name::parse("h.local") }),
                Record::new(instance, TTL, true, RData::Txt(dns::txt(&[("name", "x\u{7}y")]))),
            ],
            ..Message::default()
        };
        assert!(heard(&msg, from(), Instant::now()).0.is_empty());
        let mut hostile = reply(&a, Kinds::ALL, TTL, true, &[], &[]);
        for r in &mut hostile.answers {
            if let RData::Txt(t) = &mut r.data {
                *t = dns::txt(&[("id", "ab<c>d"), ("name", "evil\u{1b}[31m"), ("quic", "99999"), ("ca", "nothex")]);
            }
        }
        let (seen, _) = heard(&hostile, from(), Instant::now());
        let i = seen.first().ok_or_else(|| anyhow::anyhow!("not heard"))?;
        assert_eq!(i.id, "abcd");
        assert_eq!(i.name, "evil[31m");
        assert_eq!((i.quic, i.ca.clone()), (0, None));
        Ok(())
    }

    fn query(name: Name, qtype: u16, unicast: bool) -> Message {
        Message { questions: vec![Question { name, qtype, qclass: dns::CLASS_IN, unicast }], ..Message::default() }
    }

    #[test]
    fn queries_are_answered_with_what_they_ask() {
        let a = advert();
        let (k, u) = wanted(&a, &query(service(), dns::TYPE_PTR, false));
        assert_eq!((k, u), (Kinds { ptr: true, ..Kinds::default() }, false));
        let (k, u) = wanted(&a, &query(service(), dns::TYPE_PTR, true));
        assert!(k.ptr && u);
        let (k, _) = wanted(&a, &query(a.instance_name(), dns::TYPE_ANY, false));
        assert!(k.srv && k.txt && !k.ptr);
        let (k, _) = wanted(&a, &query(a.host_name(), dns::TYPE_A, false));
        assert!(k.addr && !k.srv);
        let (k, _) = wanted(&a, &query(Name::parse(SERVICE_TYPES), dns::TYPE_PTR, false));
        assert!(k.types);
        let (k, _) = wanted(&a, &query(Name::parse("_http._tcp.local"), dns::TYPE_PTR, false));
        assert!(!k.any());
        // Known-answer suppression: a fresh PTR in the query silences ours; a stale one does not.
        for (ttl, suppressed) in [(TTL, true), (TTL / 4, false)] {
            let mut q = query(service(), dns::TYPE_PTR, false);
            q.answers.push(Record::new(service(), ttl, false, RData::Ptr(a.instance_name())));
            assert_eq!(wanted(&a, &q).0.ptr, !suppressed);
        }
        // The PTR answer carries the rest as additionals.
        let r = reply(&a, Kinds { ptr: true, ..Kinds::default() }, TTL, true, &[Ipv4Addr::LOCALHOST], &[]);
        assert_eq!(r.answers.len(), 1);
        assert_eq!(r.additionals.len(), 3);
        assert!(r.answers.iter().all(|r| !r.cache_flush));
    }

    #[test]
    fn legacy_replies_echo_the_question() -> anyhow::Result<()> {
        let a = advert();
        let mut q = query(service(), dns::TYPE_PTR, false);
        q.id = 0x4242;
        let r = legacy_reply(&a, &q, Kinds { ptr: true, ..Kinds::default() }, &[], &[]);
        let r = Message::decode(&r.encode()?)?;
        assert_eq!(r.id, 0x4242);
        assert_eq!(r.questions, q.questions);
        assert!(r.records().all(|x| x.ttl <= LEGACY_TTL && !x.cache_flush));
        Ok(())
    }

    #[test]
    fn instance_names_fit_a_label() {
        let mut a = advert();
        a.name = "ü".repeat(80);
        let label = a.instance_label();
        assert!(label.len() <= 63, "{} bytes", label.len());
        assert!(label.ends_with("(1a2b3c4d)"));
        assert!(Message { answers: vec![Record::new(a.instance_name(), 1, false, RData::A(Ipv4Addr::LOCALHOST))], ..Message::default() }
            .encode()
            .is_ok());
    }

    fn instance(id: &str, label: &str, now: Instant, ttl: u64) -> Instance {
        Instance {
            id: id.into(),
            instance: label.into(),
            name: "box".into(),
            role: "server".into(),
            version: "0.1.0".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            port: 7070,
            quic: 7071,
            ca: None,
            enrolled: None,
            addresses: vec![IpAddr::V4(Ipv4Addr::new(192, 168, 1, 80))],
            first_seen: 1,
            last_seen: 1,
            expires: now + Duration::from_secs(ttl),
        }
    }

    #[test]
    fn the_table_tracks_changes_and_expiry() {
        let t = Table::default();
        let now = Instant::now();
        assert!(t.upsert(instance("a", "A", now, 120)));
        // A refresh is not a change; a new version is.
        let mut again = instance("a", "A", now, 120);
        again.last_seen = 99;
        assert!(!t.upsert(again));
        let mut newer = instance("a", "A", now, 120);
        newer.version = "0.2.0".into();
        assert!(t.upsert(newer));
        assert_eq!(t.snapshot().first().map(|i| i.first_seen), Some(1));
        // The same instance name under a new id replaces the old entry.
        assert!(t.upsert(instance("b", "A", now, 120)));
        assert_eq!(t.len(), 1);
        assert!(t.upsert(instance("c", "C", now, 10)));
        assert!(!t.sweep(now + Duration::from_secs(5)));
        assert!(t.sweep(now + Duration::from_secs(11)));
        assert_eq!(t.snapshot().iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), vec!["b"]);
        assert!(t.forget_instance("A"));
        assert!(t.is_empty());
        // Bounded: the entry closest to expiring makes room.
        for n in 0..MAX_INSTANCES + 5 {
            t.upsert(instance(&format!("i{n}"), &format!("I{n}"), now, 100 + u64::try_from(n).unwrap_or(0)));
        }
        assert_eq!(t.len(), MAX_INSTANCES);
        assert!(t.snapshot().iter().all(|i| i.id != "i0"));
    }

    #[test]
    fn instance_ids_persist() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let a = instance_id(dir.path());
        assert_eq!(a.len(), 32);
        assert_eq!(instance_id(dir.path()), a);
        std::fs::write(dir.path().join("instance.id"), "not an id")?;
        assert_ne!(instance_id(dir.path()), a);
        assert_eq!(ca_fingerprint(b"x").len(), 16);
        Ok(())
    }

    #[test]
    fn subnets() {
        let m = Ipv4Addr::new(255, 255, 255, 0);
        assert!(same_subnet(Ipv4Addr::new(192, 168, 1, 77), m, Ipv4Addr::new(192, 168, 1, 3)));
        assert!(!same_subnet(Ipv4Addr::new(192, 168, 1, 77), m, Ipv4Addr::new(192, 168, 2, 3)));
        assert!(is_virtual("docker0") && is_virtual("br-1a2b3c") && !is_virtual("br0") && !is_virtual("enp5s0"));
        assert_eq!(local_address_for(IpAddr::V4(Ipv4Addr::LOCALHOST)), Some(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    }

    /// The engine on real hyper-rt sockets, over loopback unicast (no multicast needed): a legacy
    /// query from another port is answered, and the answer is heard as the instance.
    #[test]
    fn the_engine_answers_unicast_queries() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let a = advert();
            let opts = Options {
                advert: Some(Arc::new(Mutex::new(a.clone()))),
                port: 0,
                multicast: false,
                ..Options::browse_only()
            };
            let engine = Engine::open(opts, Arc::new(Table::default()))?;
            let port = engine.local_addr().map(|s| s.port()).ok_or_else(|| anyhow::anyhow!("no port"))?;
            let stop = Shutdown::new();
            let served = {
                let stop = stop.clone();
                crate::rt::spawn(async move { engine.serve(stop, &|| {}).await })
            };
            let client = hyper_rt::udp::UdpSocket::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).map_err(|e| anyhow::anyhow!("{e}"))?;
            let mut q = query(service(), dns::TYPE_PTR, false);
            q.id = 77;
            client
                .send_to_writable(&q.encode()?, SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let mut buf = vec![0u8; dns::MAX_MESSAGE];
            let (n, from) = crate::rt::timeout(Duration::from_secs(5), client.recv_from(&mut buf))
                .await
                .map_err(|_| anyhow::anyhow!("no answer"))?
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let reply = Message::decode(buf.get(..n).unwrap_or_default())?;
            assert_eq!(reply.id, 77);
            let (seen, _) = heard(&reply, from, Instant::now());
            assert_eq!(seen.first().map(|i| i.id.as_str()), Some(a.id.as_str()));
            assert_eq!(seen.first().and_then(Instance::url), Some(format!("http://127.0.0.1:{}", a.port)));
            // Garbage is ignored and the engine keeps answering.
            client.send_to_writable(&[0xff; 40], SocketAddr::from((Ipv4Addr::LOCALHOST, port))).await.map_err(|e| anyhow::anyhow!("{e}"))?;
            client.send_to_writable(&q.encode()?, SocketAddr::from((Ipv4Addr::LOCALHOST, port))).await.map_err(|e| anyhow::anyhow!("{e}"))?;
            let again = crate::rt::timeout(Duration::from_secs(5), client.recv_from(&mut buf)).await;
            assert!(matches!(again, Ok(Ok(_))));
            stop.cancel();
            let finished = crate::rt::timeout(Duration::from_secs(5), served).await;
            assert!(matches!(finished, Ok(Ok(Ok(())))));
            Ok(())
        })
    }
}
