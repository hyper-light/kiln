//! Unicast DNS-SD (RFC 6763 on ordinary DNS, `via: dns-sd`): `_kiln._tcp.<domain>` looked up
//! through the name servers, for networks where multicast does not reach (routed subnets, client
//! isolation, VPNs) but DNS does.
//!
//! * **Domains.** `[discovery] domains`, the system's search domains, and the browse domains those
//!   list (`b._dns-sd._udp.<domain>` and `lb._dns-sd._udp.<domain>`, RFC 6763 §11, also asked for
//!   each LAN subnet's reverse zone). `local` is multicast DNS's, never asked here.
//! * **Name servers.** `[discovery] dns_servers`, else `/etc/resolv.conf`'s (Unix), else the network
//!   adapters' (Windows).
//! * **Queries.** UDP, with recursion desired and a 1232-byte EDNS(0) buffer; the answer must come
//!   from the server asked, with the query's id and question. A truncated answer (TC) is asked
//!   again over TCP (two-byte length framing, RFC 1035 §4.2.2); a server that does not do EDNS
//!   (FORMERR, NOTIMP) is asked again without it. Answers are decoded by [`crate::dns`], with its
//!   limits.
//! * **Browsing.** PTR → each instance's SRV and TXT (from the PTR answer's additionals when the
//!   server sent them, else asked) → the SRV target's A and AAAA. Every five minutes; entries stay
//!   two rounds.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::dns::{self, Message, Name, RData, Record};
use crate::rt::Shutdown;

use super::{Discovery, Instance, SERVICE_TYPE, Via};

pub const BROWSE_EVERY: Duration = Duration::from_secs(300);
/// How long what a browse found stays listed: two rounds and a minute.
const HOLD: Duration = Duration::from_secs(660);
pub const UDP_TIMEOUT: Duration = Duration::from_millis(1500);
pub const TCP_TIMEOUT: Duration = Duration::from_secs(4);
const MAX_DOMAINS: usize = 16;
const MAX_SERVERS: usize = 3;
const MAX_PER_DOMAIN: usize = 32;

/// A name server address: `192.168.1.1`, `192.168.1.1:5353`, `fd00::1`, `[fd00::1]:53`.
pub fn parse_server(s: &str) -> Option<SocketAddr> {
    let s = s.trim();
    if let Ok(a) = s.parse::<SocketAddr>() {
        return Some(a);
    }
    // An IPv6 address may carry a zone (`fe80::1%eth0`): kept out, the scope is not usable here.
    let bare = s.split('%').next().unwrap_or(s);
    bare.parse::<IpAddr>().ok().map(|ip| SocketAddr::new(ip, 53))
}

/// The name servers and search domains of a `resolv.conf`.
pub fn parse_resolv_conf(text: &str) -> (Vec<SocketAddr>, Vec<String>) {
    let mut servers = Vec::new();
    let mut search = Vec::new();
    for line in text.lines() {
        let line = line.split(['#', ';']).next().unwrap_or_default();
        let mut words = line.split_whitespace();
        match words.next() {
            Some("nameserver") => {
                if let Some(a) = words.next().and_then(parse_server)
                    && !servers.contains(&a)
                {
                    servers.push(a);
                }
            }
            // The last `search` or `domain` line wins (resolv.conf(5)).
            Some("search" | "domain") => search = words.map(str::to_owned).collect(),
            _ => {}
        }
    }
    (servers, search)
}

/// The system's name servers and search domains.
#[cfg(unix)]
pub fn system_resolver() -> (Vec<SocketAddr>, Vec<String>) {
    std::fs::read_to_string("/etc/resolv.conf").map(|t| parse_resolv_conf(&t)).unwrap_or_default()
}

/// The system's name servers and search domains: those of the adapters that are up.
#[cfg(windows)]
pub fn system_resolver() -> (Vec<SocketAddr>, Vec<String>) {
    let mut servers = Vec::new();
    let mut search: Vec<String> = Vec::new();
    for a in super::winnet::adapters().unwrap_or_default().into_iter().filter(|a| a.up) {
        for ip in a.dns_servers {
            // fec0:0:0:ffff::1-3 are Windows' unconfigured IPv6 placeholders.
            let placeholder = matches!(ip, IpAddr::V6(v6) if v6.segments().first() == Some(&0xfec0));
            let addr = SocketAddr::new(ip, 53);
            if !placeholder && !servers.contains(&addr) {
                servers.push(addr);
            }
        }
        for s in a.dns_suffixes {
            if !search.contains(&s) {
                search.push(s);
            }
        }
    }
    (servers, search)
}

#[cfg(not(any(unix, windows)))]
pub fn system_resolver() -> (Vec<SocketAddr>, Vec<String>) {
    (Vec::new(), Vec::new())
}

/// The name servers to ask: `[discovery] dns_servers`, else the system's.
pub fn name_servers(settings: &crate::config::Discovery) -> Vec<SocketAddr> {
    let configured: Vec<SocketAddr> = settings.dns_servers.iter().filter_map(|s| parse_server(s)).collect();
    let mut servers = if configured.is_empty() { system_resolver().0 } else { configured };
    servers.truncate(MAX_SERVERS);
    servers
}

/// A domain worth browsing: a plausible host name, not multicast DNS's `local`.
pub fn browsable(domain: &str) -> Option<Name> {
    let d = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    let name = Name::parse(&d);
    let ok_label = |l: &String| !l.is_empty() && l.len() <= 63 && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    let ok = !name.labels().is_empty() && name.wire_len() <= 255 && name.labels().iter().all(ok_label) && !name.is_under(&Name::parse("local"));
    ok.then_some(name)
}

/// The reverse zone of an IPv4 subnet's address, where a network may list its browse domains
/// (RFC 6763 §11): `0.1.168.192.in-addr.arpa`.
pub fn reverse_zone(net: Ipv4Addr) -> Name {
    let o = net.octets();
    Name::from_labels(o.iter().rev().map(u8::to_string).chain(["in-addr".to_owned(), "arpa".to_owned()]))
}

// ------------------------------------------------------------------------------ asking

fn id_of(data: &[u8]) -> Option<u16> {
    Some(u16::from_be_bytes([*data.first()?, *data.get(1)?]))
}

/// One exchange with `server` over UDP: the answer with `id`, from that server. A truncated
/// answer is asked again over TCP.
pub async fn exchange(server: SocketAddr, query: &[u8], id: u16, timeout: Duration) -> Result<Vec<u8>, String> {
    let bind: SocketAddr = if server.is_ipv4() { (Ipv4Addr::UNSPECIFIED, 0).into() } else { (Ipv6Addr::UNSPECIFIED, 0).into() };
    let sock = hyper_rt::udp::UdpSocket::bind(bind).map_err(|e| format!("UDP socket: {e}"))?;
    sock.send_to_writable(query, server).await.map_err(|e| format!("send to {server}: {e}"))?;
    let deadline = Instant::now() + timeout;
    let mut buf = vec![0u8; 4096];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(format!("no answer from {server}"));
        }
        let (n, from) = match crate::rt::timeout(left, sock.recv_from(&mut buf)).await {
            Err(_) => return Err(format!("no answer from {server}")),
            Ok(Err(e)) => return Err(format!("{server}: {e}")),
            Ok(Ok(got)) => got,
        };
        let data = buf.get(..n).unwrap_or_default();
        // Only the server asked, with the query's id (anything else is spoofed or stale).
        if from != server || id_of(data) != Some(id) || data.len() < 12 {
            continue;
        }
        let flags = u16::from_be_bytes([data.get(2).copied().unwrap_or(0), data.get(3).copied().unwrap_or(0)]);
        if flags & dns::FLAG_TRUNCATED != 0 {
            return exchange_tcp(server, query, id, TCP_TIMEOUT).await;
        }
        return Ok(data.to_vec());
    }
}

async fn read_exact(stream: &hyper_rt::tcp::TcpStream, buf: &mut [u8]) -> Result<(), String> {
    let mut at = 0;
    while at < buf.len() {
        let n = stream.read(buf.get_mut(at..).unwrap_or_default()).await.map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Err("the server closed the connection".to_owned());
        }
        at += n;
    }
    Ok(())
}

/// One exchange over TCP: the message with a two-byte length before it, both ways.
pub async fn exchange_tcp(server: SocketAddr, query: &[u8], id: u16, timeout: Duration) -> Result<Vec<u8>, String> {
    let len = u16::try_from(query.len()).map_err(|_| "query too large".to_owned())?;
    let work = async {
        let stream = hyper_rt::tcp::TcpStream::connect(server).await.map_err(|e| format!("TCP {server}: {e}"))?;
        let mut framed = Vec::with_capacity(query.len() + 2);
        framed.extend_from_slice(&len.to_be_bytes());
        framed.extend_from_slice(query);
        stream.write_all(&framed).await.map_err(|e| format!("TCP {server}: {e}"))?;
        let mut head = [0u8; 2];
        read_exact(&stream, &mut head).await?;
        let mut body = vec![0u8; usize::from(u16::from_be_bytes(head))];
        read_exact(&stream, &mut body).await?;
        if id_of(&body) != Some(id) {
            return Err(format!("{server} answered another query over TCP"));
        }
        Ok(body)
    };
    crate::rt::timeout(timeout, work).await.map_err(|_| format!("{server}: no answer over TCP"))?
}

/// Ask the name servers one question: the first answer that is no server failure. NXDOMAIN is
/// an answer (an empty one).
pub async fn query(servers: &[SocketAddr], name: &Name, qtype: u16) -> Result<Message, String> {
    let mut last = format!("no name server to ask for {name}");
    for server in servers.iter().take(MAX_SERVERS) {
        let mut edns = true;
        // A lost datagram is asked again once; a server that does not do EDNS, again without it.
        for _attempt in 0..3 {
            let id: u16 = rand::random();
            let bytes = Message::query(id, name.clone(), qtype, edns).encode().map_err(|e| format!("{name}: {e}"))?;
            let answer = match exchange(*server, &bytes, id, UDP_TIMEOUT).await.and_then(|b| Message::decode(&b).map_err(|e| format!("{server}: {e}"))) {
                Ok(m) => m,
                Err(e) => {
                    last = e;
                    continue;
                }
            };
            let asked = answer.questions.first().is_none_or(|q| q.name == *name && q.qtype == qtype);
            if !answer.is_response() || !asked {
                last = format!("{server} answered another question");
                continue;
            }
            match answer.rcode() {
                dns::RCODE_NOERROR | dns::RCODE_NXDOMAIN => return Ok(answer),
                dns::RCODE_FORMERR | dns::RCODE_NOTIMP if edns => edns = false,
                code => {
                    last = format!("{server}: {}", dns::rcode_name(code));
                    break;
                }
            }
        }
    }
    Err(last)
}

/// The targets of `name`'s PTR records in an answer.
fn ptr_targets(m: &Message, name: &Name) -> Vec<Name> {
    m.answers
        .iter()
        .filter_map(|r| match &r.data {
            RData::Ptr(t) if r.name == *name => Some(t.clone()),
            _ => None,
        })
        .collect()
}

/// The browse domains `domain` lists (`b._dns-sd._udp` and `lb._dns-sd._udp`).
pub async fn browse_domains(servers: &[SocketAddr], domain: &Name) -> Vec<Name> {
    let mut out: Vec<Name> = Vec::new();
    for kind in ["b", "lb"] {
        let name = Name::from_labels([kind, "_dns-sd", "_udp"]).join(domain);
        if let Ok(m) = query(servers, &name, dns::TYPE_PTR).await {
            for t in ptr_targets(&m, &name) {
                if !out.contains(&t) && browsable(&t.to_zone(false)).is_some() {
                    out.push(t);
                }
            }
        }
    }
    out
}

/// The kiln instances `_kiln._tcp.<domain>` lists.
pub async fn browse(servers: &[SocketAddr], domain: &Name, now: Instant) -> Result<Vec<Instance>, String> {
    let service = Name::parse(SERVICE_TYPE).join(domain);
    let ptrs = query(servers, &service, dns::TYPE_PTR).await?;
    let mut out = Vec::new();
    for instance in ptr_targets(&ptrs, &service).into_iter().filter(|t| t.parent() == service).take(MAX_PER_DOMAIN) {
        // What the PTR answer brought along, else asked.
        let mut known: Vec<Record> = ptrs.records().cloned().collect();
        let srv_of = |rs: &[Record]| {
            rs.iter().find_map(|r| match &r.data {
                RData::Srv { port, target, .. } if r.name == instance => Some((*port, target.clone())),
                _ => None,
            })
        };
        let txt_of = |rs: &[Record]| {
            rs.iter().find_map(|r| match &r.data {
                RData::Txt(strings) if r.name == instance => Some(dns::txt_pairs(strings)),
                _ => None,
            })
        };
        if srv_of(&known).is_none()
            && let Ok(m) = query(servers, &instance, dns::TYPE_SRV).await
        {
            known.extend(m.answers);
        }
        if txt_of(&known).is_none()
            && let Ok(m) = query(servers, &instance, dns::TYPE_TXT).await
        {
            known.extend(m.answers);
        }
        let (Some((port, target)), Some(pairs)) = (srv_of(&known), txt_of(&known)) else { continue };
        let address_of = |r: &Record| match r.data {
            RData::A(ip) => Some(IpAddr::V4(ip)),
            RData::Aaaa(ip) => Some(IpAddr::V6(ip)),
            _ => None,
        };
        let mut addresses: Vec<IpAddr> = known.iter().filter(|r| r.name == target).filter_map(address_of).collect();
        if addresses.is_empty() {
            for qtype in [dns::TYPE_A, dns::TYPE_AAAA] {
                // The answer section holds the target's records, after any CNAMEs.
                if let Ok(m) = query(servers, &target, qtype).await {
                    addresses.extend(m.answers.iter().filter_map(address_of));
                }
            }
        }
        let label = instance.first().unwrap_or_default().to_owned();
        if let Some(i) = super::instance_from(&label, port, &pairs, addresses, Via::DnsSd, now, HOLD) {
            out.push(i);
        }
    }
    Ok(out)
}

/// The domains to browse: configured, search, and the browse domains those (and the LAN subnets'
/// reverse zones) list.
async fn domains(d: &Discovery, servers: &[SocketAddr], search: &[String]) -> Vec<Name> {
    let mut out: Vec<Name> = Vec::new();
    for s in d.settings.domains.iter().chain(search) {
        if let Some(n) = browsable(s)
            && !out.contains(&n)
        {
            out.push(n);
        }
    }
    let mut listing: Vec<Name> = out.clone();
    for net in super::subnets(&d.settings.interfaces) {
        if let Some((IpAddr::V4(ip), _)) = super::parse_cidr(&net) {
            listing.push(reverse_zone(ip));
        }
    }
    for zone in listing.iter().take(MAX_DOMAINS) {
        for found in browse_domains(servers, zone).await {
            if !out.contains(&found) {
                out.push(found);
            }
        }
    }
    out.truncate(MAX_DOMAINS);
    out
}

/// One round: every domain browsed, the table and the method's health updated.
async fn browse_all(d: &Discovery, on_change: &(dyn Fn() + Send + Sync)) {
    let (system_servers, search) = super::unicast::system_resolver();
    let configured: Vec<SocketAddr> = d.settings.dns_servers.iter().filter_map(|s| parse_server(s)).collect();
    let mut servers = if configured.is_empty() { system_servers } else { configured };
    servers.truncate(MAX_SERVERS);
    let domains = if servers.is_empty() {
        d.settings.domains.iter().chain(&search).filter_map(|s| browsable(s)).collect()
    } else {
        domains(d, &servers, &search).await
    };
    let now = Instant::now();
    let mut errors = Vec::new();
    let mut answered = 0usize;
    if !servers.is_empty() {
        for domain in &domains {
            match browse(&servers, domain, now).await {
                Ok(found) => {
                    answered += 1;
                    d.found(found, Via::DnsSd, on_change);
                }
                Err(e) => errors.push(format!("{}: {e}", domain.to_zone(false))),
            }
        }
    }
    let mut h = d.health.lock();
    h.dns_sd.ran = true;
    h.dns_sd.domains = domains.iter().map(|n| n.to_zone(false)).collect();
    h.dns_sd.servers = servers.iter().map(|s| if s.port() == 53 { s.ip().to_string() } else { s.to_string() }).collect();
    h.dns_sd.error = (answered == 0 && !errors.is_empty()).then(|| errors.join("; "));
    h.dns_sd.at = Some(crate::util::now_unix());
}

/// Browse unicast DNS-SD until `shutdown`: soon after start (at once when only looking for a
/// moment), then every five minutes.
pub async fn run(d: &Arc<Discovery>, shutdown: &Shutdown, on_change: &Arc<dyn Fn() + Send + Sync>) {
    if !d.browsing() {
        return;
    }
    if !d.oneshot && !shutdown.sleep(Duration::from_secs(2)).await {
        return;
    }
    loop {
        // A round may take a while against unreachable servers; stopping does not wait for it.
        if !super::until_stopped(shutdown, browse_all(d, &**on_change)).await || !shutdown.sleep(BROWSE_EVERY).await {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolv_conf() {
        let (servers, search) = parse_resolv_conf(
            "# generated\nnameserver 192.168.1.1\nnameserver fd00::53 # ULA\nnameserver fe80::1%eth0\nnameserver nonsense\n\
             domain old.example\nsearch home.example lan ; trailing\noptions ndots:2\nnameserver 192.168.1.1\n",
        );
        assert_eq!(
            servers,
            vec![
                "192.168.1.1:53".parse().unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0))),
                "[fd00::53]:53".parse().unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0))),
                "[fe80::1]:53".parse().unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0))),
            ]
        );
        assert_eq!(search, vec!["home.example".to_owned(), "lan".to_owned()]);
        assert_eq!(parse_server("10.0.0.1:5300").map(|a| a.port()), Some(5300));
        assert_eq!(parse_server("[::1]:53").map(|a| a.ip()), Some(IpAddr::V6(Ipv6Addr::LOCALHOST)));
    }

    #[test]
    fn browse_domains_and_reverse_zones() {
        assert_eq!(browsable("Home.Example."), Some(Name::parse("home.example")));
        assert_eq!(browsable("local"), None);
        assert_eq!(browsable("x.local."), None);
        assert_eq!(browsable("bad name.example"), None);
        assert_eq!(browsable(""), None);
        assert_eq!(reverse_zone(Ipv4Addr::new(192, 168, 1, 0)).to_zone(true), "0.1.168.192.in-addr.arpa.");
    }
}
