//! Home-network discovery: kiln servers (and agents started with `--discover`) announce themselves
//! as the DNS-SD service `_kiln._tcp` (RFC 6763) and keep a table of the other kiln instances they
//! find. A server shows that table in the UI ("On this network"), and `kiln join` uses it to find a
//! server without being told its address. Every way of finding an instance feeds the same
//! [`Table`], and each entry says how it was found ([`Via`]):
//!
//! * **Multicast DNS** (`mdns`, this file): kiln's own responder and browser on UDP 5353
//!   (RFC 6762). The default on Linux, and the fallback where the system has no service discovery.
//! * **The system's service discovery** (`native`, [`native`]): Bonjour on macOS, DNS-SD on
//!   Windows 10 1809+. Preferred there: the system owns port 5353 and its firewall exemptions.
//! * **Unicast DNS-SD** (`dns-sd`, [`unicast`]): `_kiln._tcp.<domain>` looked up through ordinary
//!   name servers, for the configured domains, the search domains and the browse domains they list;
//!   [`publish`] adds this instance's records to a zone with DNS UPDATE signed with TSIG.
//! * **Probes** (`scan`, `probe`, `seed`, `tailscale`, [`probe`]): one-shot multicast DNS queries
//!   sent straight to an address (answered by unicast, so they cross routers), and `GET /api/me` on
//!   kiln's HTTP port, for networks where multicast does not reach.
//!
//! The multicast DNS engine:
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
//!   engine stops. Queries from outside this machine's subnets are answered only when their source
//!   is allowed by `[discovery] answer_from` (default: private addresses).
//! * **Browsing.** Queries for `_kiln._tcp.local.` at start (with known answers), and every
//!   response heard is folded into the [`Table`]. An entry expires when its records' TTL (120 s,
//!   four missed announcements) runs out, or at once on a goodbye.
//! * **Names.** Instances are `kiln <role> on <host> (<id8>)` and their host names
//!   `kiln-<id8>.local`, both from a random instance id kept on disk, so no probing or conflict
//!   resolution is needed and kiln never claims the host's own `.local` name (avahi's).

pub mod invite;
mod native;
pub mod probe;
pub mod publish;
pub mod status;
pub mod unicast;

#[cfg(target_os = "macos")]
mod bonjour;
#[cfg(windows)]
mod windns;
#[cfg(windows)]
mod winnet;

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
/// The service type without its domain (`_kiln._tcp.<domain>` in unicast DNS-SD).
pub const SERVICE_TYPE: &str = "_kiln._tcp";
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
/// The TXT keys kiln announces (and reads).
pub const TXT_KEYS: [&str; 9] = ["id", "name", "role", "version", "quic", "ca", "enrolled", "os", "arch"];
/// The port the HTTP API usually listens on (probes try it when no port is given).
pub const HTTP_PORT: u16 = 7070;

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

/// How an instance was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub enum Via {
    /// kiln's own multicast DNS.
    #[serde(rename = "mdns")]
    Mdns,
    /// The system's service discovery (Bonjour, Windows DNS-SD).
    #[serde(rename = "native")]
    Native,
    /// Unicast DNS-SD through name servers.
    #[serde(rename = "dns-sd")]
    DnsSd,
    /// A network scan (`POST /api/discovery/scan`), and its re-probes.
    #[serde(rename = "scan")]
    Scan,
    /// A probe of one address (`POST /api/discovery/probe`), and its re-probes.
    #[serde(rename = "probe")]
    Probe,
    /// A probe of a Tailscale peer.
    #[serde(rename = "tailscale")]
    Tailscale,
    /// A probe of a configured seed address.
    #[serde(rename = "seed")]
    Seed,
}

impl Via {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mdns => "mdns",
            Self::Native => "native",
            Self::DnsSd => "dns-sd",
            Self::Scan => "scan",
            Self::Probe => "probe",
            Self::Tailscale => "tailscale",
            Self::Seed => "seed",
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

    pub fn short_id(&self) -> &str {
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

    /// The first label of the host name the SRV record points at: `kiln-1a2b3c4d`.
    pub fn host_label(&self) -> String {
        format!("kiln-{}", self.short_id())
    }

    fn host_name(&self) -> Name {
        Name::from_labels([self.host_label(), "local".to_owned()])
    }

    /// The TXT record's `key=value` pairs, in order ([`TXT_KEYS`]).
    pub fn txt_pairs(&self) -> Vec<(String, String)> {
        let mut pairs = vec![
            ("id".to_owned(), self.id.clone()),
            ("name".to_owned(), self.name.clone()),
            ("role".to_owned(), self.role.as_str().to_owned()),
            ("version".to_owned(), self.version.clone()),
            ("quic".to_owned(), self.quic.to_string()),
            ("os".to_owned(), self.os.clone()),
            ("arch".to_owned(), self.arch.clone()),
        ];
        if let Some(ca) = &self.ca {
            pairs.push(("ca".to_owned(), ca.clone()));
        }
        if let Some(e) = &self.enrolled {
            pairs.push(("enrolled".to_owned(), e.clone()));
        }
        pairs
    }

    fn txt(&self) -> Vec<Vec<u8>> {
        let pairs = self.txt_pairs();
        let borrowed: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        dns::txt(&borrowed)
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

/// A response with A and AAAA records for host names: each `(name, qtype)` gets the addresses its
/// type asks for (ANY: both).
fn host_reply(asked: &[(&Name, u16)], ttl: u32, cache_flush: bool, v4: &[Ipv4Addr], v6: &[Ipv6Addr]) -> Message {
    let mut answers = Vec::new();
    for (name, qtype) in asked {
        if matches!(*qtype, dns::TYPE_A | dns::TYPE_ANY) {
            answers.extend(v4.iter().map(|ip| Record::new((*name).clone(), ttl, cache_flush, RData::A(*ip))));
        }
        if matches!(*qtype, dns::TYPE_AAAA | dns::TYPE_ANY) {
            answers.extend(v6.iter().map(|ip| Record::new((*name).clone(), ttl, cache_flush, RData::Aaaa(*ip))));
        }
    }
    Message { id: 0, flags: dns::FLAG_RESPONSE | dns::FLAG_AUTHORITATIVE, answers, ..Message::default() }
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
    /// Its addresses in the order first heard: the IPv4 address its packets came from, then its
    /// A/AAAA records. Addresses heard over several packets (interfaces) are merged, and each one
    /// lapses with the TTL it was last announced with.
    pub addresses: Vec<IpAddr>,
    pub first_seen: i64,
    pub last_seen: i64,
    /// How it was found; each way lapses on its own.
    pub via: Vec<Via>,
    #[serde(skip)]
    pub expires: Instant,
    /// When each address lapses.
    #[serde(skip)]
    address_expiry: Vec<(IpAddr, Instant)>,
    /// When each way it was found lapses.
    #[serde(skip)]
    sources: Vec<(Via, Instant)>,
    /// Known only from the HTTP answer of an older server, which has no instance id to give: the id
    /// is made up, and a real sighting of the same server replaces the entry.
    #[serde(skip)]
    pub(crate) synthetic: bool,
}

impl Instance {
    /// Fold an earlier sighting's addresses into this one: known addresses keep their place,
    /// lapsed ones go.
    fn merge_addresses(&mut self, earlier: &Instance, now: Instant) {
        let mut merged: Vec<(IpAddr, Instant)> = earlier.address_expiry.iter().copied().filter(|(_, t)| *t > now).collect();
        for (ip, t) in &self.address_expiry {
            match merged.iter_mut().find(|(have, _)| have == ip) {
                Some(slot) => slot.1 = slot.1.max(*t),
                None => merged.push((*ip, *t)),
            }
        }
        merged.truncate(16);
        self.addresses = merged.iter().map(|(ip, _)| *ip).collect();
        self.address_expiry = merged;
    }

    /// Fold an earlier sighting's ways of being found into this one; lapsed ones go.
    fn merge_sources(&mut self, earlier: &Instance, now: Instant) {
        let mut merged: Vec<(Via, Instant)> = earlier.sources.iter().copied().filter(|(_, t)| *t > now).collect();
        for (via, t) in &self.sources {
            match merged.iter_mut().find(|(have, _)| have == via) {
                Some(slot) => slot.1 = slot.1.max(*t),
                None => merged.push((*via, *t)),
            }
        }
        self.set_sources(merged);
    }

    fn set_sources(&mut self, mut sources: Vec<(Via, Instant)>) {
        sources.sort_by_key(|(v, _)| *v);
        if let Some(latest) = sources.iter().map(|(_, t)| *t).max() {
            self.expires = latest;
        }
        self.via = sources.iter().map(|(v, _)| *v).collect();
        self.sources = sources;
    }

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
            && (self.port, self.quic, &self.ca, &self.enrolled, &self.addresses, &self.via)
                == (o.port, o.quic, &o.ca, &o.enrolled, &o.addresses, &o.via)
    }
}

/// Printable text from the network, at most `max` characters.
fn clean(s: Option<&str>, max: usize) -> String {
    s.unwrap_or_default().chars().filter(|c| !c.is_control()).take(max).collect()
}

fn hex16(s: &str) -> Option<String> {
    (s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit())).then(|| s.to_ascii_lowercase())
}

/// An instance from what its service records say, however they were found: the instance label,
/// the SRV port, the TXT pairs and the addresses, good for `ttl`. `None` without an id.
pub(crate) fn instance_from(
    label: &str,
    port: u16,
    pairs: &[(String, String)],
    addresses: Vec<IpAddr>,
    via: Via,
    now: Instant,
    ttl: Duration,
) -> Option<Instance> {
    let get = |k: &str| pairs.iter().find(|(key, _)| key.eq_ignore_ascii_case(k)).map(|(_, v)| v.as_str());
    let id: String = get("id")?.chars().filter(char::is_ascii_alphanumeric).take(64).collect();
    if id.is_empty() {
        return None;
    }
    let expires = now + ttl;
    let mut addrs: Vec<IpAddr> = Vec::new();
    for ip in addresses {
        if !ip.is_unspecified() && !addrs.contains(&ip) && addrs.len() < 16 {
            addrs.push(ip);
        }
    }
    let unix = crate::util::now_unix();
    Some(Instance {
        id,
        instance: clean(Some(label), 63),
        name: clean(get("name"), 64),
        role: clean(get("role"), 16),
        version: clean(get("version"), 32),
        os: clean(get("os"), 32),
        arch: clean(get("arch"), 32),
        port,
        quic: get("quic").and_then(|q| q.parse().ok()).unwrap_or(0),
        ca: get("ca").and_then(hex16),
        enrolled: get("enrolled").and_then(hex16),
        address_expiry: addrs.iter().map(|ip| (*ip, expires)).collect(),
        addresses: addrs,
        first_seen: unix,
        last_seen: unix,
        via: vec![via],
        expires,
        sources: vec![(via, expires)],
        synthetic: false,
    })
}

/// What a response tells about kiln instances: those it describes (complete with SRV and TXT,
/// TTL > 0) and the instance names it says goodbye to (TTL 0).
fn heard(msg: &Message, from: SocketAddr, now: Instant) -> (Vec<Instance>, Vec<String>) {
    heard_via(msg, from, now, Via::Mdns)
}

fn heard_via(msg: &Message, from: SocketAddr, now: Instant, via: Via) -> (Vec<Instance>, Vec<String>) {
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
        let ttl = Duration::from_secs(u64::from(srv_ttl.min(txt_ttl).clamp(1, 4500)));
        // The source address, when it can be dialed as is (an IPv6 link-local source carries no
        // usable scope here), then the address records.
        let mut addresses: Vec<IpAddr> = Vec::new();
        let source = from.ip();
        if !source.is_unspecified() && !matches!(source, IpAddr::V6(v6) if v6.is_unicast_link_local()) {
            addresses.push(source);
        }
        for r in msg.records().filter(|r| r.name == target && r.ttl > 0) {
            match r.data {
                RData::A(ip) => addresses.push(IpAddr::V4(ip)),
                RData::Aaaa(ip) => addresses.push(IpAddr::V6(ip)),
                _ => {}
            }
        }
        if let Some(i) = instance_from(&label, port, &pairs, addresses, via, now, ttl) {
            seen.push(i);
        }
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

    pub fn get(&self, id: &str) -> Option<Instance> {
        self.inner.lock().get(id).cloned()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().is_empty()
    }

    /// How many instances were found `via`.
    pub fn count_via(&self, via: Via) -> usize {
        self.inner.lock().values().filter(|i| i.via.contains(&via)).count()
    }

    /// Record a sighting; true when the instance is new or something about it changed.
    pub(crate) fn upsert(&self, mut seen: Instance, now: Instant) -> bool {
        let mut g = self.inner.lock();
        if let Some(have) = g.get_mut(&seen.id) {
            seen.merge_addresses(have, now);
            seen.merge_sources(have, now);
            let changed = !have.same(&seen);
            seen.first_seen = have.first_seen;
            *have = seen;
            return changed;
        }
        let overlaps = |a: &Instance, b: &Instance| a.port == b.port && a.addresses.iter().any(|x| b.addresses.contains(x));
        // An older server known only by its HTTP answer is already listed under its real id.
        if seen.synthetic && g.values().any(|i| !i.synthetic && overlaps(i, &seen)) {
            return false;
        }
        // The same instance name under a new id (a reinstall) is stale, and so is a made-up entry
        // for the server now heard properly.
        g.retain(|_, i| i.instance != seen.instance && !(i.synthetic && !seen.synthetic && overlaps(i, &seen)));
        if g.len() >= MAX_INSTANCES
            && let Some(oldest) = g.values().min_by_key(|i| i.expires).map(|i| i.id.clone())
        {
            g.remove(&oldest);
        }
        g.insert(seen.id.clone(), seen);
        true
    }

    /// The instance called `label` is no longer found these ways (a goodbye, a removal); it
    /// goes when nothing else still finds it.
    pub(crate) fn forget(&self, label: &str, vias: &[Via]) -> bool {
        let mut g = self.inner.lock();
        let mut changed = false;
        g.retain(|_, i| {
            if i.instance != label {
                return true;
            }
            let left: Vec<(Via, Instant)> = i.sources.iter().copied().filter(|(v, _)| !vias.contains(v)).collect();
            if left.len() != i.sources.len() {
                changed = true;
                i.set_sources(left);
            }
            !i.sources.is_empty()
        });
        changed
    }

    /// Drop the ways of finding each instance that lapsed, and instances nothing finds any more.
    pub(crate) fn sweep(&self, now: Instant) -> bool {
        let mut g = self.inner.lock();
        let mut changed = false;
        g.retain(|_, i| {
            if i.sources.iter().any(|(_, t)| *t <= now) {
                let left: Vec<(Via, Instant)> = i.sources.iter().copied().filter(|(_, t)| *t > now).collect();
                i.set_sources(left);
                changed = true;
            }
            !i.sources.is_empty()
        });
        changed
    }

    /// Instance names heard over multicast DNS with the seconds their records have left (known
    /// answers for a query).
    fn known(&self, now: Instant) -> Vec<(String, u32)> {
        self.inner
            .lock()
            .values()
            .filter_map(|i| {
                let until = i.sources.iter().find(|(v, _)| *v == Via::Mdns)?.1;
                Some((i.instance.clone(), u32::try_from(until.saturating_duration_since(now).as_secs()).unwrap_or(0)))
            })
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

/// Container and VM plumbing rather than a LAN (Windows has `is_virtual_windows`).
#[cfg(any(unix, test))]
fn is_virtual(name: &str) -> bool {
    const PLUMBING: [&str; 14] = [
        "docker", "veth", "br-", "virbr", "vnet", "cni", "flannel", "cali", "cilium", "vxlan", "kube", "lxcbr", "lxdbr", "podman",
    ];
    PLUMBING.iter().any(|p| name.starts_with(p))
}

/// The interfaces to use: `allow` when given, else every interface that is up, running and
/// multicast-capable, except loopback, point-to-point links (VPNs) and container/VM bridges.
#[cfg(unix)]
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

/// Virtual adapters on Windows (Hyper-V and WSL switches, VM host-only networks, VPN tunnels),
/// by name or description.
#[cfg(any(windows, test))]
fn is_virtual_windows(name: &str, description: &str) -> bool {
    const PLUMBING: [&str; 14] = [
        "vethernet", "hyper-v", "virtualbox", "vmware", "docker", "wsl", "npcap", "loopback", "tailscale", "wireguard", "zerotier", "wintun",
        "tap-windows", "openvpn",
    ];
    let (n, d) = (name.to_ascii_lowercase(), description.to_ascii_lowercase());
    PLUMBING.iter().any(|p| n.contains(p) || d.contains(p))
}

/// The interfaces to use on Windows (`GetAdaptersAddresses`), by the same rules as on Unix:
/// `allow` (friendly names or adapter GUIDs) when given, else every adapter that is up and
/// multicast-capable, except loopback, point-to-point and tunnel adapters and virtual switches.
#[cfg(windows)]
pub fn interfaces(allow: &[String]) -> Vec<Iface> {
    let adapters = match winnet::adapters() {
        Ok(a) => a,
        Err(e) => {
            tracing::debug!(error = %e, "cannot list the network adapters");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for a in adapters {
        if allow.is_empty() {
            let tunnel = matches!(a.if_type, winnet::IF_TYPE_PPP | winnet::IF_TYPE_TUNNEL | winnet::IF_TYPE_PROP_VIRTUAL);
            if !a.up || !a.multicast || a.if_type == winnet::IF_TYPE_SOFTWARE_LOOPBACK || tunnel || is_virtual_windows(&a.name, &a.description) {
                continue;
            }
        } else if !allow.iter().any(|n| *n == a.name || *n == a.id) {
            continue;
        }
        let index = if a.index != 0 { a.index } else { a.ipv6_index };
        let v4 = a.v4.iter().map(|(ip, len)| (*ip, mask_of(*len))).collect();
        out.push(Iface { name: a.name, index, v4, v6: a.v6 });
    }
    out.retain(|i| i.index != 0 && (!i.v4.is_empty() || !i.v6.is_empty()));
    out
}

/// The netmask of a prefix length.
fn mask_of(len: u8) -> Ipv4Addr {
    match len {
        0 => Ipv4Addr::UNSPECIFIED,
        l if l >= 32 => Ipv4Addr::BROADCAST,
        l => Ipv4Addr::from(u32::MAX << (32 - u32::from(l))),
    }
}

fn prefix_len(mask: Ipv4Addr) -> u8 {
    u8::try_from(u32::from(mask).leading_ones()).unwrap_or(32)
}

fn same_subnet(addr: Ipv4Addr, mask: Ipv4Addr, other: Ipv4Addr) -> bool {
    u32::from(addr) & u32::from(mask) == u32::from(other) & u32::from(mask)
}

/// The largest range a scan covers in one piece.
pub const MAX_SCAN_PREFIX: u8 = 22;

/// The IPv4 subnets of the interfaces in use (`192.168.1.0/24`), each at most a /22: a larger
/// one is narrowed to the /22 around this machine's address. What a scan covers by default.
pub fn subnets(allow: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for i in interfaces(allow) {
        for (addr, mask) in i.v4 {
            if addr.is_loopback() || addr.is_link_local() {
                continue;
            }
            let len = prefix_len(mask).max(MAX_SCAN_PREFIX);
            let net = Ipv4Addr::from(u32::from(addr) & u32::from(mask_of(len)));
            let s = format!("{net}/{len}");
            if !out.contains(&s) {
                out.push(s);
            }
        }
    }
    out
}

/// This machine's address on the route to `remote` (no packet is sent).
pub fn local_address_for(remote: IpAddr) -> Option<IpAddr> {
    let bind: SocketAddr = if remote.is_ipv4() { (Ipv4Addr::UNSPECIFIED, 0).into() } else { (Ipv6Addr::UNSPECIFIED, 0).into() };
    let s = std::net::UdpSocket::bind(bind).ok()?;
    s.connect((remote, 9)).ok()?;
    s.local_addr().ok().map(|a| a.ip()).filter(|ip| !ip.is_unspecified())
}

/// An address or a CIDR (`10.0.0.0/8`, `fd00::/8`, `192.168.1.7`).
pub fn parse_cidr(s: &str) -> Option<(IpAddr, u8)> {
    let s = s.trim();
    let (ip, len) = match s.split_once('/') {
        Some((ip, len)) => (ip.parse::<IpAddr>().ok()?, len.parse::<u8>().ok()?),
        None => {
            let ip = s.parse::<IpAddr>().ok()?;
            (ip, if ip.is_ipv4() { 32 } else { 128 })
        }
    };
    let max = if ip.is_ipv4() { 32 } else { 128 };
    (len <= max).then_some((ip, len))
}

/// Whether `ip` lies in `net`/`len`.
pub fn in_net(ip: IpAddr, net: IpAddr, len: u8) -> bool {
    match (ip, net) {
        (IpAddr::V4(a), IpAddr::V4(n)) => {
            let m = u32::from(mask_of(len));
            u32::from(a) & m == u32::from(n) & m
        }
        (IpAddr::V6(a), IpAddr::V6(n)) => {
            let m = if len == 0 { 0 } else { u128::MAX << (128 - u32::from(len.min(128))) };
            u128::from(a) & m == u128::from(n) & m
        }
        (IpAddr::V6(a), IpAddr::V4(_)) => a.to_ipv4_mapped().is_some_and(|v4| in_net(IpAddr::V4(v4), net, len)),
        (IpAddr::V4(_), IpAddr::V6(_)) => false,
    }
}

/// Private and link-local ranges: RFC 1918, 100.64.0.0/10 (carrier-grade NAT, Tailscale),
/// 169.254.0.0/16, fc00::/7, fe80::/10, and loopback.
pub fn is_private(ip: IpAddr) -> bool {
    const RANGES: [(&str, u8); 9] = [
        ("10.0.0.0", 8),
        ("172.16.0.0", 12),
        ("192.168.0.0", 16),
        ("100.64.0.0", 10),
        ("169.254.0.0", 16),
        ("127.0.0.0", 8),
        ("fc00::", 7),
        ("fe80::", 10),
        ("::1", 128),
    ];
    RANGES.iter().any(|(net, len)| net.parse::<IpAddr>().is_ok_and(|n| in_net(ip, n, *len)))
}

/// Which sources from outside this machine's subnets the responder answers (`[discovery]
/// answer_from`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AnswerFrom {
    any: bool,
    private: bool,
    nets: Vec<(IpAddr, u8)>,
}

impl AnswerFrom {
    /// The rules, and the entries that are none of `private`, `any` or an address/CIDR.
    pub fn parse(list: &[String]) -> (Self, Vec<String>) {
        let mut a = Self::default();
        let mut bad = Vec::new();
        for s in list {
            match s.trim().to_ascii_lowercase().as_str() {
                "private" => a.private = true,
                "any" | "all" => a.any = true,
                other => match parse_cidr(other) {
                    Some(net) => a.nets.push(net),
                    None => bad.push(s.clone()),
                },
            }
        }
        (a, bad)
    }

    pub fn allows(&self, ip: IpAddr) -> bool {
        self.any || (self.private && is_private(ip)) || self.nets.iter().any(|(n, l)| in_net(ip, *n, *l))
    }
}

// ------------------------------------------------------------------------------ the engine

/// How a multicast DNS engine runs.
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
    /// Who outside this machine's subnets may query by unicast.
    pub answer_from: AnswerFrom,
    /// Where the engine reports how it runs.
    pub health: Arc<Mutex<status::Health>>,
    /// More host names it answers A and AAAA for with this machine's addresses (a front door's
    /// `<name>.local`): unique records, answered at once and announced with the advert.
    pub hosts: Arc<Mutex<Vec<Name>>>,
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
            answer_from: AnswerFrom::parse(&["private".to_owned()]).0,
            health: Arc::default(),
            hosts: Arc::default(),
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
    /// When a packet from another host was last noted (at most once a second).
    noted_foreign: Option<Instant>,
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
            noted_foreign: None,
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
        self.opts.health.lock().mdns.interfaces = self.interface_names();
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

    /// The interface a packet from `from` arrived on, by subnet (IPv4) or scope or /64 (IPv6).
    fn iface_for(&self, from: SocketAddr) -> Option<&Iface> {
        match from {
            SocketAddr::V4(a) => self.ifaces.iter().find(|i| i.v4.iter().any(|(addr, mask)| same_subnet(*addr, *mask, *a.ip()))),
            SocketAddr::V6(a) => self.ifaces.iter().find(|i| {
                (a.scope_id() != 0 && i.index == a.scope_id())
                    || i.v6.iter().any(|ours| in_net(IpAddr::V6(*a.ip()), IpAddr::V6(*ours), 64) && !ours.is_unicast_link_local())
            }),
        }
    }

    /// One of this machine's own addresses.
    fn is_own(&self, ip: IpAddr) -> bool {
        ip.is_loopback()
            || self.ifaces.iter().any(|i| match ip {
                IpAddr::V4(v4) => i.v4.iter().any(|(a, _)| *a == v4),
                IpAddr::V6(v6) => i.v6.contains(&v6),
            })
    }

    /// Whether a query from `from` may be answered: this host, a link-local source, a source on one
    /// of our subnets, or one `[discovery] answer_from` allows (RFC 6762 §11 keeps mDNS on the
    /// link; probes from other networks are let in explicitly).
    fn may_answer(&self, from: SocketAddr) -> bool {
        let ip = from.ip();
        let link_local = match ip {
            IpAddr::V4(v4) => v4.is_link_local(),
            IpAddr::V6(v6) => v6.is_unicast_link_local(),
        };
        self.is_own(ip) || link_local || self.iface_for(from).is_some() || self.opts.answer_from.allows(ip)
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
        if let Some(advert) = self.advert() {
            self.multicast(None, now, false, |v4, v6| reply(&advert, Kinds::ALL, ttl, true, v4, v6));
        }
        let hosts = self.opts.hosts.lock().clone();
        if !hosts.is_empty() {
            let all: Vec<(&Name, u16)> = hosts.iter().map(|h| (h, dns::TYPE_ANY)).collect();
            self.multicast(None, now, false, |v4, v6| host_reply(&all, ttl, true, v4, v6));
        }
        self.announced = ttl > 0;
    }

    /// Whether there is anything to announce.
    fn announces(&self) -> bool {
        self.opts.advert.is_some() || !self.opts.hosts.lock().is_empty()
    }

    /// Questions for the extra host names (a front door's `<name>.local`): unique records,
    /// answered at once (RFC 6762 §6) on the interface asked, by unicast to legacy and QU
    /// questions.
    fn answer_hosts(&mut self, q: &Message, from: SocketAddr, now: Instant) {
        let hosts = self.opts.hosts.lock().clone();
        if hosts.is_empty() {
            return;
        }
        let asked: Vec<(&Name, u16)> = q
            .questions
            .iter()
            .filter_map(|qq| {
                let host = hosts.iter().find(|h| **h == qq.name)?;
                matches!(qq.qtype, dns::TYPE_A | dns::TYPE_AAAA | dns::TYPE_ANY).then_some((host, qq.qtype))
            })
            .collect();
        if asked.is_empty() || !self.may_answer(from) {
            return;
        }
        let iface = self.iface_for(from).cloned();
        let (v4, v6) = iface.as_ref().map_or_else(|| self.all_addrs(), |i| self.addrs(i));
        let unicast = q.questions.iter().any(|qq| qq.unicast);
        if from.port() != MDNS_PORT {
            let mut m = host_reply(&asked, LEGACY_TTL, false, &v4, &v6);
            if m.answers.is_empty() {
                return;
            }
            m.id = q.id;
            m.questions = q.questions.clone();
            self.send(&m, from, None);
        } else if unicast {
            let m = host_reply(&asked, TTL, true, &v4, &v6);
            if !m.answers.is_empty() {
                self.send(&m, from, None);
            }
        } else {
            self.multicast(iface.map(|i| i.index), now, false, |v4, v6| host_reply(&asked, TTL, true, v4, v6));
        }
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

    /// Any packet from another host shows multicast reaches us (the "silent network" check).
    fn note_traffic(&mut self, from: SocketAddr, now: Instant) {
        if self.is_own(from.ip()) || self.noted_foreign.is_some_and(|t| now.saturating_duration_since(t) < Duration::from_secs(1)) {
            return;
        }
        self.noted_foreign = Some(now);
        self.opts.health.lock().mdns.last_foreign = Some(now);
    }

    /// Handle one datagram. True when the table changed.
    fn on_packet(&mut self, data: &[u8], from: SocketAddr, now: Instant) -> bool {
        let Ok(msg) = Message::decode(data) else {
            tracing::trace!(%from, "undecodable multicast DNS packet");
            return false;
        };
        self.note_traffic(from, now);
        if msg.is_response() {
            if !self.opts.browse {
                return false;
            }
            let me = self.advert().map(|a| a.id);
            let (seen, gone) = heard(&msg, from, now);
            let mut changed = false;
            let mut any = false;
            for i in seen {
                if me.as_deref() != Some(i.id.as_str()) {
                    any = true;
                    changed |= self.table.upsert(i, now);
                }
            }
            for label in gone {
                changed |= self.table.forget(&label, &[Via::Mdns]);
            }
            if any {
                self.opts.health.lock().heard(Via::Mdns);
            }
            return changed;
        }
        // A standard query (opcode 0) about us, from where we answer?
        if msg.flags & dns::OPCODE_MASK != 0 {
            return false;
        }
        self.answer_hosts(&msg, from, now);
        let Some(advert) = self.advert() else { return false };
        let (kinds, unicast) = wanted(&advert, &msg);
        if !kinds.any() {
            return false;
        }
        if !self.may_answer(from) {
            tracing::debug!(%from, "multicast DNS query from outside the allowed networks ignored ([discovery] answer_from)");
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
        let mut next_announce = self.announces().then_some(start);
        let mut announcements = 0u32;
        let mut next_query = self.opts.browse.then_some(start);
        let mut queries = 0u32;
        let mut next_ifaces = start + INTERFACES_EVERY;
        let mut buf4 = vec![0u8; dns::MAX_MESSAGE];
        let mut buf6 = vec![0u8; dns::MAX_MESSAGE];
        let mut errors = 0u32;
        let changed = self.opts.changed.clone();
        loop {
            let deadline = [next_announce, next_query, Some(next_ifaces), self.pending.as_ref().map(|p| p.due)]
                .into_iter()
                .flatten()
                .min()
                .unwrap_or(next_ifaces);
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
                    next_announce = self.announces().then_some(now);
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
            if next_ifaces <= now {
                if self.refresh_interfaces() && self.announces() {
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

/// Run multicast DNS until `shutdown`. Without a usable port 5353 or multicast network kiln works
/// on without it and tries again every minute; a failure while serving is returned.
pub async fn run(opts: Options, table: Arc<Table>, shutdown: Shutdown, on_change: Arc<dyn Fn() + Send + Sync>) -> anyhow::Result<()> {
    let mut warned = false;
    let health = Arc::clone(&opts.health);
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
                {
                    let mut h = health.lock();
                    h.mdns.state = status::MdnsState::Running;
                    h.mdns.since = Some(Instant::now());
                }
                let served = engine.serve(shutdown, &*on_change).await;
                if let Err(e) = &served {
                    health.lock().mdns.state = status::MdnsState::Failed(format!("{e:#}"));
                }
                return served;
            }
            Err(e) => {
                health.lock().mdns.state = status::MdnsState::PortBusy(format!("{e:#}"));
                if warned {
                    tracing::debug!(error = %format!("{e:#}"), "discovery still unavailable");
                } else {
                    tracing::warn!(error = %format!("{e:#}"), "multicast DNS unavailable (UDP 5353); retrying every minute");
                    warned = true;
                }
                if !shutdown.sleep(Duration::from_secs(60)).await {
                    return Ok(());
                }
            }
        }
    }
}

/// Look for kiln instances for `wait` with every method the configuration allows (multicast DNS
/// or the system's discovery, unicast DNS-SD, seeds, Tailscale). Multicast DNS shares port 5353,
/// or, when that port cannot be shared, sends one-shot queries from another port whose answers
/// come back by unicast.
pub async fn browse_once(wait: Duration, interfaces: &[String]) -> anyhow::Result<Vec<Instance>> {
    let mut settings = crate::config::Config::load(&crate::config::default_config_path()).map(|c| c.discovery).unwrap_or_default();
    if !interfaces.is_empty() {
        settings.interfaces = interfaces.to_vec();
    }
    let d = Arc::new(Discovery::for_browsing(settings));
    let stop = Shutdown::new();
    let timer = {
        let stop = stop.clone();
        async move {
            crate::rt::sleep(wait).await;
            stop.cancel();
        }
    };
    let noop: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
    futures::join!(d.run(&stop, &noop), timer);
    if let status::MdnsState::Failed(e) = &d.health.lock().mdns.state {
        tracing::debug!(error = %e, "multicast DNS failed while browsing");
    }
    Ok(d.table.snapshot())
}

// ------------------------------------------------------------------------------ the whole thing

/// One kiln instance's discovery: what it announces, the instances it found, how each method is
/// doing, and (on a server) the invitations, scans and probes the API drives. Shared by the server
/// (`hub.discovery`), `kiln agent --discover` and `kiln join`.
pub struct Discovery {
    pub settings: crate::config::Discovery,
    pub advert: Arc<Mutex<Advert>>,
    pub table: Arc<Table>,
    pub changed: Arc<Notify>,
    /// How each method is doing.
    pub health: Arc<Mutex<status::Health>>,
    /// Instances this server invited to join (`POST /api/discovery/invite`).
    pub invites: invite::Invites,
    /// Scans and the addresses probes found instances at.
    pub probes: probe::Probes,
    /// The HTTP listener accepts IPv6, so AAAA records are worth announcing.
    ipv6: bool,
    /// Only browsing for a moment (`kiln join`): no retries, a one-shot querier when port 5353
    /// cannot be shared.
    oneshot: bool,
}

fn port_of(addr: &str) -> Option<u16> {
    addr.rsplit_once(':').and_then(|(_, p)| p.trim_end_matches('/').parse().ok())
}

/// Run `work` unless `shutdown` comes first: true when it finished.
pub(crate) async fn until_stopped(shutdown: &Shutdown, work: impl std::future::Future<Output = ()>) -> bool {
    let work = work.fuse();
    let stop = shutdown.cancelled().fuse();
    futures::pin_mut!(work, stop);
    futures::select_biased! {
        () = stop => false,
        () = work => true,
    }
}

impl Discovery {
    fn new(settings: crate::config::Discovery, advert: Advert, ipv6: bool, oneshot: bool) -> Self {
        let health = status::Health::new(&settings);
        Self {
            settings,
            advert: Arc::new(Mutex::new(advert)),
            table: Arc::new(Table::default()),
            changed: Arc::new(Notify::new()),
            health: Arc::new(Mutex::new(health)),
            invites: invite::Invites::default(),
            probes: probe::Probes::default(),
            ipv6,
            oneshot,
        }
    }

    pub fn for_server(cfg: &crate::config::Config, ca_der: &[u8]) -> Self {
        let http = port_of(&cfg.server.http_addr).unwrap_or(HTTP_PORT);
        let quic = port_of(&cfg.server.quic_advertise).or_else(|| port_of(&cfg.server.quic_addr)).unwrap_or(7071);
        let mut advert = Advert::new(instance_id(&cfg.server.data_dir), Role::Server, http, quic);
        advert.ca = Some(ca_fingerprint(ca_der));
        Self::new(cfg.discovery.clone(), advert, cfg.server.http_addr.starts_with('['), false)
    }

    /// An agent's (`--discover`): it announces itself and looks for servers whatever `announce`
    /// and `browse` say, since the flag asked for it.
    pub fn for_agent(state_dir: &Path, mut settings: crate::config::Discovery, enrolled_ca_der: Option<&[u8]>) -> Self {
        let mut advert = Advert::new(instance_id(state_dir), Role::Agent, 0, 0);
        advert.enrolled = enrolled_ca_der.map(ca_fingerprint);
        settings.enabled = true;
        settings.announce = true;
        settings.browse = true;
        Self::new(settings, advert, false, false)
    }

    /// Only looking, for a moment (`kiln join`, `kiln up`): announces nothing and publishes nothing.
    pub fn for_browsing(mut settings: crate::config::Discovery) -> Self {
        settings.enabled = true;
        settings.announce = false;
        settings.browse = true;
        settings.publish = None;
        let advert = Advert::new(hex::encode(rand::random::<[u8; 16]>()), Role::Agent, 0, 0);
        Self::new(settings, advert, false, true)
    }

    pub fn announcing(&self) -> bool {
        self.settings.enabled && self.settings.announce
    }

    pub fn browsing(&self) -> bool {
        self.settings.enabled && self.settings.browse
    }

    /// The multicast DNS engine's options.
    pub fn options(&self) -> Options {
        Options {
            advert: self.announcing().then(|| Arc::clone(&self.advert)),
            browse: self.browsing(),
            interfaces: self.settings.interfaces.clone(),
            ipv6_addresses: self.ipv6,
            port: MDNS_PORT,
            multicast: true,
            changed: Arc::clone(&self.changed),
            answer_from: AnswerFrom::parse(&self.settings.answer_from).0,
            health: Arc::clone(&self.health),
            hosts: Arc::default(),
        }
    }

    /// Record instances found `via`, telling `on_change` when the table changed.
    pub(crate) fn found(&self, seen: Vec<Instance>, via: Via, on_change: &(dyn Fn() + Send + Sync)) -> Vec<String> {
        let me = self.advert.lock().id.clone();
        let now = Instant::now();
        let mut changed = false;
        let mut ids = Vec::new();
        for i in seen {
            if i.id == me {
                continue;
            }
            ids.push(i.id.clone());
            changed |= self.table.upsert(i, now);
        }
        if !ids.is_empty() {
            self.health.lock().heard(via);
        }
        if changed {
            on_change();
        }
        ids
    }

    /// Run every discovery method until `shutdown`: multicast DNS (or the system's service
    /// discovery), unicast DNS-SD browsing and publishing, probes of seeds, Tailscale peers and
    /// what scans found, and the table's expiry. A method that fails is retried on its own; the
    /// others carry on.
    pub async fn run(self: &Arc<Self>, shutdown: &Shutdown, on_change: &Arc<dyn Fn() + Send + Sync>) {
        futures::join!(
            self.run_multicast(shutdown, on_change),
            unicast::run(self, shutdown, on_change),
            publish::run(self, shutdown),
            probe::run(self, shutdown, on_change),
            self.sweep_loop(shutdown, on_change),
        );
    }

    async fn sweep_loop(&self, shutdown: &Shutdown, on_change: &Arc<dyn Fn() + Send + Sync>) {
        while shutdown.sleep(SWEEP_EVERY).await {
            if self.table.sweep(Instant::now()) {
                on_change();
            }
            self.invites.purge(Instant::now());
        }
    }

    /// Multicast DNS: the system's service discovery where there is one (and it is allowed),
    /// else kiln's own engine.
    async fn run_multicast(self: &Arc<Self>, shutdown: &Shutdown, on_change: &Arc<dyn Fn() + Send + Sync>) {
        if !self.announcing() && !self.browsing() {
            self.health.lock().mdns.state = status::MdnsState::Off("discovery is turned off".to_owned());
            return;
        }
        if native::SUPPORTED && self.settings.native {
            match native::run(self, shutdown, on_change).await {
                Ok(()) => return,
                Err(e) => {
                    tracing::info!(error = %format!("{e:#}"), "the system's service discovery is unavailable; using kiln's own multicast DNS");
                    self.health.lock().native.state = status::NativeState::Unavailable(format!("{e:#}"));
                }
            }
        }
        if self.oneshot {
            self.browse_multicast_once(shutdown, on_change).await;
            return;
        }
        loop {
            match run(self.options(), Arc::clone(&self.table), shutdown.clone(), Arc::clone(on_change)).await {
                Ok(()) => return,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "multicast DNS stopped; restarting in 30 s");
                    if !shutdown.sleep(Duration::from_secs(30)).await {
                        return;
                    }
                }
            }
        }
    }

    /// `kiln join`'s look around: share port 5353, or query from another port when it is taken.
    async fn browse_multicast_once(&self, shutdown: &Shutdown, on_change: &Arc<dyn Fn() + Send + Sync>) {
        let mut opts = self.options();
        let engine = match Engine::open(opts.clone(), Arc::clone(&self.table)) {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(error = %format!("{e:#}"), "port 5353 unavailable; querying from another port");
                opts.port = 0;
                match Engine::open(opts, Arc::clone(&self.table)) {
                    Ok(e) => e,
                    Err(e) => {
                        self.health.lock().mdns.state = status::MdnsState::PortBusy(format!("{e:#}"));
                        return;
                    }
                }
            }
        };
        self.health.lock().mdns.state = status::MdnsState::Running;
        if let Err(e) = engine.serve(shutdown.clone(), &**on_change).await {
            self.health.lock().mdns.state = status::MdnsState::Failed(format!("{e:#}"));
        }
    }

    /// The server's discovery child: every method, telling UI clients when the table changes.
    pub async fn serve(hub: Arc<crate::hub::Hub>) -> anyhow::Result<()> {
        let weak = Arc::downgrade(&hub);
        let on_change: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            if let Some(h) = weak.upgrade() {
                h.emit(&crate::wire::events::ServerEvent::Discovery);
            }
        });
        let d = Arc::clone(&hub.discovery);
        d.run(&hub.shutdown, &on_change).await;
        Ok(())
    }
}

/// An agent started with `--discover`: announced as role `agent` while it runs, and looking for
/// servers (to join one when it is not enrolled yet).
pub struct AgentAnnouncer {
    pub discovery: Arc<Discovery>,
    pub table: Arc<Table>,
    task: crate::rt::JoinHandle<()>,
}

impl AgentAnnouncer {
    pub fn start(state_dir: &Path, enrolled_ca_der: Option<&[u8]>, settings: crate::config::Discovery, shutdown: Shutdown) -> Self {
        let discovery = Arc::new(Discovery::for_agent(state_dir, settings, enrolled_ca_der));
        let table = Arc::clone(&discovery.table);
        let d = Arc::clone(&discovery);
        let task = crate::rt::spawn(async move {
            let noop: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
            d.run(&shutdown, &noop).await;
        });
        Self { discovery, table, task }
    }

    /// This agent's instance id (what a server invites).
    pub fn id(&self) -> String {
        self.discovery.advert.lock().id.clone()
    }

    /// The agent enrolled: say so in its announcements.
    pub fn enrolled(&self, ca_der: &[u8]) {
        self.discovery.advert.lock().enrolled = Some(ca_fingerprint(ca_der));
        self.discovery.changed.notify();
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
        assert_eq!(i.via, vec![Via::Mdns]);
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
        let pairs = vec![
            ("id".to_owned(), id.to_owned()),
            ("name".to_owned(), "box".to_owned()),
            ("role".to_owned(), "server".to_owned()),
            ("version".to_owned(), "0.1.0".to_owned()),
            ("quic".to_owned(), "7071".to_owned()),
            ("os".to_owned(), "linux".to_owned()),
            ("arch".to_owned(), "x86_64".to_owned()),
        ];
        let mut i = instance_from(label, 7070, &pairs, vec![IpAddr::V4(Ipv4Addr::new(192, 168, 1, 80))], Via::Mdns, now, Duration::from_secs(ttl))
            .unwrap_or_else(|| Instance {
                id: String::new(),
                instance: String::new(),
                name: String::new(),
                role: String::new(),
                version: String::new(),
                os: String::new(),
                arch: String::new(),
                port: 0,
                quic: 0,
                ca: None,
                enrolled: None,
                addresses: Vec::new(),
                first_seen: 0,
                last_seen: 0,
                via: Vec::new(),
                expires: now,
                address_expiry: Vec::new(),
                sources: Vec::new(),
                synthetic: false,
            });
        i.first_seen = 1;
        i.last_seen = 1;
        i
    }

    #[test]
    fn the_table_tracks_changes_and_expiry() {
        let t = Table::default();
        let now = Instant::now();
        assert!(t.upsert(instance("a", "A", now, 120), now));
        // A refresh is not a change; a new version is.
        let mut again = instance("a", "A", now, 120);
        again.last_seen = 99;
        assert!(!t.upsert(again, now));
        let mut newer = instance("a", "A", now, 120);
        newer.version = "0.2.0".into();
        assert!(t.upsert(newer, now));
        assert_eq!(t.snapshot().first().map(|i| i.first_seen), Some(1));
        // The same instance name under a new id replaces the old entry.
        assert!(t.upsert(instance("b", "A", now, 120), now));
        assert_eq!(t.len(), 1);
        assert!(t.upsert(instance("c", "C", now, 10), now));
        assert!(!t.sweep(now + Duration::from_secs(5)));
        assert!(t.sweep(now + Duration::from_secs(11)));
        assert_eq!(t.snapshot().iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), vec!["b"]);
        assert!(t.forget("A", &[Via::Mdns]));
        assert!(t.is_empty());
        // Bounded: the entry closest to expiring makes room.
        for n in 0..MAX_INSTANCES + 5 {
            t.upsert(instance(&format!("i{n}"), &format!("I{n}"), now, 100 + u64::try_from(n).unwrap_or(0)), now);
        }
        assert_eq!(t.len(), MAX_INSTANCES);
        assert!(t.snapshot().iter().all(|i| i.id != "i0"));
    }

    #[test]
    fn ways_of_finding_an_instance_merge_and_lapse_one_by_one() {
        let t = Table::default();
        let now = Instant::now();
        let by = |via: Via, ttl: u64| {
            let mut i = instance("m", "M", now, ttl);
            i.set_sources(vec![(via, now + Duration::from_secs(ttl))]);
            i
        };
        assert!(t.upsert(by(Via::Mdns, 120), now));
        // Found another way too: a change (the UI shows how), listed once.
        assert!(t.upsert(by(Via::DnsSd, 600), now));
        assert!(!t.upsert(by(Via::DnsSd, 600), now));
        assert_eq!(t.get("m").map(|i| i.via), Some(vec![Via::Mdns, Via::DnsSd]));
        assert_eq!(t.count_via(Via::DnsSd), 1);
        // A multicast goodbye leaves what unicast DNS-SD still says.
        assert!(t.forget("M", &[Via::Mdns]));
        assert_eq!(t.get("m").map(|i| i.via), Some(vec![Via::DnsSd]));
        // Each way lapses with its own TTL; the last one takes the entry.
        assert!(t.upsert(by(Via::Scan, 150), now));
        assert!(t.sweep(now + Duration::from_secs(200)));
        assert_eq!(t.get("m").map(|i| i.via), Some(vec![Via::DnsSd]));
        assert!(t.sweep(now + Duration::from_secs(601)));
        assert!(t.is_empty());
    }

    #[test]
    fn synthetic_entries_give_way_to_real_ones() {
        let t = Table::default();
        let now = Instant::now();
        let mut fake = instance("feedfacefeedface", "kiln server at 192.168.1.80:7070", now, 150);
        fake.synthetic = true;
        assert!(t.upsert(fake.clone(), now));
        // The same server heard properly replaces it; a made-up entry never shadows a real one.
        assert!(t.upsert(instance("realid", "kiln server on box (realid)", now, 120), now));
        assert_eq!(t.snapshot().iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), vec!["realid"]);
        assert!(!t.upsert(fake, now));
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn addresses_merge_across_packets_and_lapse() {
        let t = Table::default();
        let now = Instant::now();
        let at = |ip: [u8; 4], when: Instant, ttl: u64| {
            let mut i = instance("m", "M", when, ttl);
            i.addresses = vec![IpAddr::from(ip)];
            i.address_expiry = vec![(IpAddr::from(ip), when + Duration::from_secs(ttl))];
            i
        };
        // A machine on two links of ours announces on both: one entry, both addresses, in the
        // order first heard, and hearing them again in either order is no change.
        assert!(t.upsert(at([192, 168, 1, 80], now, 120), now));
        assert!(t.upsert(at([10, 0, 0, 80], now, 120), now));
        assert!(!t.upsert(at([192, 168, 1, 80], now, 120), now));
        assert!(!t.upsert(at([10, 0, 0, 80], now, 120), now));
        let addrs = t.snapshot().first().map(|i| i.addresses.clone()).unwrap_or_default();
        assert_eq!(addrs, vec![IpAddr::from([192, 168, 1, 80]), IpAddr::from([10, 0, 0, 80])]);
        // One link goes away: its address lapses once its TTL ran out.
        let later = now + Duration::from_secs(130);
        assert!(t.upsert(at([10, 0, 0, 80], later, 120), later));
        let addrs = t.snapshot().first().map(|i| i.addresses.clone()).unwrap_or_default();
        assert_eq!(addrs, vec![IpAddr::from([10, 0, 0, 80])]);
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
        assert!(is_virtual_windows("vEthernet (WSL)", "Hyper-V Virtual Ethernet Adapter"));
        assert!(is_virtual_windows("Ethernet 3", "TAP-Windows Adapter V9"));
        assert!(!is_virtual_windows("Wi-Fi", "Intel(R) Wi-Fi 6E AX211 160MHz"));
        assert_eq!(local_address_for(IpAddr::V4(Ipv4Addr::LOCALHOST)), Some(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert_eq!((mask_of(0), mask_of(24), mask_of(32), mask_of(40)), (Ipv4Addr::UNSPECIFIED, m, Ipv4Addr::BROADCAST, Ipv4Addr::BROADCAST));
        assert_eq!((prefix_len(m), prefix_len(Ipv4Addr::new(255, 255, 252, 0))), (24, 22));
    }

    #[test]
    fn who_may_query_from_other_networks() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        for private in ["10.1.2.3", "172.20.0.9", "192.168.7.7", "100.101.102.103", "169.254.1.1", "fd7a:115c:a1e0::1", "fe80::1", "::ffff:10.0.0.1"] {
            assert!(is_private(ip(private)), "{private}");
        }
        for public in ["8.8.8.8", "172.32.0.1", "100.128.0.1", "2001:db8::1", "::ffff:8.8.8.8"] {
            assert!(!is_private(ip(public)), "{public}");
        }
        let (default, bad) = AnswerFrom::parse(&["private".to_owned()]);
        assert!(bad.is_empty());
        assert!(default.allows(ip("100.64.0.1")) && !default.allows(ip("203.0.113.9")));
        let (custom, bad) = AnswerFrom::parse(&["203.0.113.0/24".into(), "2001:db8::7".into(), "nonsense".into(), "10.0.0.0/33".into()]);
        assert_eq!(bad, vec!["nonsense".to_owned(), "10.0.0.0/33".to_owned()]);
        assert!(custom.allows(ip("203.0.113.200")) && custom.allows(ip("2001:db8::7")));
        assert!(!custom.allows(ip("10.0.0.1")) && !custom.allows(ip("2001:db8::8")));
        assert!(AnswerFrom::parse(&["any".into()]).0.allows(ip("8.8.8.8")));
        assert!(!AnswerFrom::default().allows(ip("10.0.0.1")));
        assert_eq!(parse_cidr("192.168.1.0/24"), Some((ip("192.168.1.0"), 24)));
        assert_eq!(parse_cidr("::1"), Some((ip("::1"), 128)));
        assert!(in_net(ip("192.168.1.255"), ip("192.168.1.0"), 24) && !in_net(ip("192.168.2.0"), ip("192.168.1.0"), 24));
        assert!(in_net(ip("1.2.3.4"), ip("0.0.0.0"), 0) && !in_net(ip("1.2.3.4"), ip("::"), 0));
    }

    /// Runs `engine` until the returned stop is cancelled.
    fn serve(engine: Engine) -> (Shutdown, crate::rt::JoinHandle<anyhow::Result<()>>) {
        let stop = Shutdown::new();
        let served = {
            let stop = stop.clone();
            crate::rt::spawn(async move { engine.serve(stop, &|| {}).await })
        };
        (stop, served)
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
            let (stop, served) = serve(engine);
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

    /// A front door's `.local` name: answered with this machine's addresses, with or without an
    /// advert of its own.
    #[test]
    fn extra_host_names_are_answered() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let opts = Options { port: 0, multicast: false, browse: false, ..Options::browse_only() };
            opts.hosts.lock().push(Name::parse("kiln.local"));
            let mut engine = Engine::open(opts, Arc::new(Table::default()))?;
            engine.ifaces = vec![Iface { name: "lan".into(), index: 2, v4: vec![(Ipv4Addr::new(10, 9, 9, 1), Ipv4Addr::new(255, 255, 255, 0))], v6: vec![] }];
            assert!(engine.announces());
            let port = engine.local_addr().map(|s| s.port()).ok_or_else(|| anyhow::anyhow!("no port"))?;
            let (stop, served) = serve(engine);
            let client = hyper_rt::udp::UdpSocket::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).map_err(|e| anyhow::anyhow!("{e}"))?;
            let to = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
            let mut buf = vec![0u8; dns::MAX_MESSAGE];
            for (name, answered) in [("KILN.local", true), ("other.local", false), ("kiln.local", true)] {
                let mut q = query(Name::parse(name), dns::TYPE_A, false);
                q.id = 9;
                client.send_to_writable(&q.encode()?, to).await.map_err(|e| anyhow::anyhow!("{e}"))?;
                let got = crate::rt::timeout(Duration::from_millis(if answered { 5000 } else { 300 }), client.recv_from(&mut buf)).await;
                match got {
                    Ok(Ok((n, _))) => {
                        assert!(answered, "{name} is not ours");
                        let m = Message::decode(buf.get(..n).unwrap_or_default())?;
                        assert_eq!(m.id, 9);
                        assert_eq!(m.answers.iter().map(|r| r.data.clone()).collect::<Vec<_>>(), [RData::A(Ipv4Addr::new(10, 9, 9, 1))]);
                    }
                    _ => assert!(!answered, "{name} got no answer"),
                }
            }
            stop.cancel();
            let _ = crate::rt::timeout(Duration::from_secs(5), served).await;
            Ok(())
        })
    }

    /// Off-link sources are answered only as `answer_from` allows (loopback always is).
    #[test]
    fn off_link_queries_follow_answer_from() -> anyhow::Result<()> {
        let opts = Options { advert: Some(Arc::new(Mutex::new(advert()))), port: 0, multicast: false, ..Options::browse_only() };
        crate::rt::block_on(async move {
            let mut engine = Engine::open(opts, Arc::new(Table::default()))?;
            let at = |s: &str| s.parse::<SocketAddr>().unwrap_or_else(|_| SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)));
            assert!(engine.may_answer(at("127.0.0.1:40000")));
            assert!(engine.may_answer(at("100.70.1.2:40000")), "Tailscale (CGNAT) is private");
            assert!(engine.may_answer(at("[fe80::1]:40000")));
            assert!(!engine.may_answer(at("8.8.8.8:40000")));
            engine.opts.answer_from = AnswerFrom::default();
            assert!(!engine.may_answer(at("10.9.9.9:40000")));
            engine.ifaces = vec![Iface { name: "lan".into(), index: 2, v4: vec![(Ipv4Addr::new(10, 9, 9, 1), Ipv4Addr::new(255, 255, 255, 0))], v6: vec![] }];
            assert!(engine.may_answer(at("10.9.9.9:40000")), "on-link");
            Ok(())
        })
    }
}
