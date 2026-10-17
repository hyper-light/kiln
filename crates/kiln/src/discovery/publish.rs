//! Publishing this instance in a unicast DNS zone (`[discovery.publish]`), so machines that browse
//! that domain with unicast DNS-SD find it wherever multicast does not reach: DNS UPDATE (RFC 2136)
//! signed with TSIG (RFC 8945), HMAC-SHA256 by default.
//!
//! * **Records.** `_kiln._tcp.<zone> PTR <instance>`, the instance's SRV (to
//!   `kiln-<id8>.<zone>`, the HTTP port; 0 for agents) and TXT (the same keys as multicast DNS),
//!   and the host's A records (AAAA when the HTTP server listens on IPv6). The same lines, in
//!   zone-file syntax, are offered to admins who would rather add them by hand
//!   (`dns_sd.records` of `GET /api/discovery`).
//! * **Updates.** Each one deletes what the instance's names held (and the PTR of an earlier
//!   instance name, after a rename) and adds the current set, so it can be repeated: at start, when
//!   something changed, and every five minutes. On shutdown the records are deleted.
//! * **Signatures.** The update carries a TSIG record over the message and the TSIG variables
//!   (key and algorithm names in canonical form, time signed, fudge 300 s); the answer's signature
//!   is checked against the request's MAC before its result is believed. Status:
//!   `dns_sd.publish`.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::dns::{self, Message, Name, Question, RData, Record};
use crate::rt::Shutdown;

use super::{Advert, Discovery, SERVICE_TYPE};

/// TTL of the published records.
pub const RECORD_TTL: u32 = 300;
/// Every update is repeated this often (and sooner when something changed).
pub const REFRESH_EVERY: Duration = Duration::from_secs(300);
const CHECK_EVERY: Duration = Duration::from_secs(60);
/// The clock skew a signature tolerates (RFC 8945 recommends 300 s).
pub const FUDGE: u16 = 300;

// ------------------------------------------------------------------------------ TSIG

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Algorithm {
    HmacSha256,
    HmacSha384,
    HmacSha512,
}

impl Algorithm {
    /// `hmac-sha256` (or empty), `hmac-sha384`, `hmac-sha512`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().trim_end_matches('.').to_ascii_lowercase().as_str() {
            "" | "hmac-sha256" => Some(Self::HmacSha256),
            "hmac-sha384" => Some(Self::HmacSha384),
            "hmac-sha512" => Some(Self::HmacSha512),
            _ => None,
        }
    }

    pub fn name(self) -> Name {
        Name::parse(match self {
            Self::HmacSha256 => "hmac-sha256",
            Self::HmacSha384 => "hmac-sha384",
            Self::HmacSha512 => "hmac-sha512",
        })
    }

    fn mac(self, key: &[u8], data: &[u8]) -> Vec<u8> {
        use hmac::{KeyInit as _, Mac as _};
        let out = match self {
            Self::HmacSha256 => hmac::Hmac::<sha2::Sha256>::new_from_slice(key).map(|mut m| {
                m.update(data);
                m.finalize().into_bytes().to_vec()
            }),
            Self::HmacSha384 => hmac::Hmac::<sha2::Sha384>::new_from_slice(key).map(|mut m| {
                m.update(data);
                m.finalize().into_bytes().to_vec()
            }),
            Self::HmacSha512 => hmac::Hmac::<sha2::Sha512>::new_from_slice(key).map(|mut m| {
                m.update(data);
                m.finalize().into_bytes().to_vec()
            }),
        };
        out.unwrap_or_default()
    }
}

/// A TSIG key.
#[derive(Clone, PartialEq, Eq)]
pub struct Key {
    pub name: Name,
    pub algorithm: Algorithm,
    pub secret: Vec<u8>,
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Key").field("name", &self.name).field("algorithm", &self.algorithm).finish_non_exhaustive()
    }
}

/// The base64 secret of a key file, and the algorithm it names: the secret alone, or a BIND
/// `key "name" { algorithm hmac-sha256; secret "…"; };` block as `tsig-keygen` writes it.
pub fn parse_secret(text: &str) -> Option<(Vec<u8>, Option<String>)> {
    use base64::Engine as _;
    let quoted_after = |word: &str| {
        let at = text.find(word)?;
        let rest = text.get(at + word.len()..)?;
        let open = rest.find('"')?;
        let rest = rest.get(open + 1..)?;
        rest.get(..rest.find('"')?).map(str::to_owned)
    };
    let algorithm = text.find("algorithm").and_then(|at| {
        let rest = text.get(at + "algorithm".len()..)?;
        rest.split([';', '\n']).next().map(|a| a.trim().trim_matches('"').to_owned())
    });
    let b64: String = match quoted_after("secret") {
        Some(s) => s,
        None => text.trim().trim_matches('"').trim_end_matches(';').to_owned(),
    }
    .chars()
    .filter(|c| !c.is_whitespace())
    .collect();
    let secret = base64::engine::general_purpose::STANDARD.decode(b64.as_bytes()).ok().filter(|s| !s.is_empty())?;
    Some((secret, algorithm.filter(|a| !a.is_empty())))
}

impl Key {
    /// The key `[discovery.publish]` names.
    pub fn load(p: &crate::config::Publish) -> Result<Self, String> {
        if p.key_name.trim().is_empty() {
            return Err("[discovery.publish] key_name is empty".to_owned());
        }
        let text = std::fs::read_to_string(&p.key_secret_file).map_err(|e| format!("cannot read {}: {e}", p.key_secret_file.display()))?;
        let (secret, from_file) = parse_secret(&text).ok_or_else(|| format!("{} holds no base64 TSIG secret", p.key_secret_file.display()))?;
        let wanted = if p.algorithm.trim().is_empty() { from_file.unwrap_or_default() } else { p.algorithm.clone() };
        let algorithm = Algorithm::parse(&wanted).ok_or_else(|| format!("unsupported TSIG algorithm {wanted:?} (hmac-sha256, hmac-sha384 or hmac-sha512)"))?;
        Ok(Self { name: Name::parse(&p.key_name), algorithm, secret })
    }
}

/// A message's TSIG record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    pub key: Name,
    pub algorithm: Name,
    pub time: u64,
    pub fudge: u16,
    pub mac: Vec<u8>,
    pub original_id: u16,
    pub error: u16,
    pub other: Vec<u8>,
}

fn u48(t: u64) -> [u8; 6] {
    let [_, _, a, b, c, d, e, f] = t.to_be_bytes();
    [a, b, c, d, e, f]
}

fn len16(n: usize) -> Result<[u8; 2], String> {
    u16::try_from(n).map(u16::to_be_bytes).map_err(|_| "TSIG field too long".to_owned())
}

/// The TSIG variables the MAC covers after the message (RFC 8945 §4.3.3).
fn variables(key: &Name, algorithm: &Name, time: u64, fudge: u16, error: u16, other: &[u8]) -> Result<Vec<u8>, String> {
    let mut v = key.canonical_wire().map_err(|e| e.to_string())?;
    v.extend_from_slice(&dns::CLASS_ANY.to_be_bytes());
    v.extend_from_slice(&0u32.to_be_bytes());
    v.extend(algorithm.canonical_wire().map_err(|e| e.to_string())?);
    v.extend_from_slice(&u48(time));
    v.extend_from_slice(&fudge.to_be_bytes());
    v.extend_from_slice(&error.to_be_bytes());
    v.extend_from_slice(&len16(other.len())?);
    v.extend_from_slice(other);
    Ok(v)
}

fn get_u16(buf: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*buf.get(at)?, *buf.get(at + 1)?]))
}

fn set_u16(buf: &mut [u8], at: usize, v: u16) -> Option<()> {
    buf.get_mut(at..at + 2)?.copy_from_slice(&v.to_be_bytes());
    Some(())
}

/// Sign an encoded message: it with a TSIG record appended (ARCOUNT + 1), and the MAC (to check
/// the answer's signature with).
pub fn sign(msg: &[u8], key: &Key, time: u64) -> Result<(Vec<u8>, Vec<u8>), String> {
    let id = get_u16(msg, 0).ok_or("message too short")?;
    let ar = get_u16(msg, 10).ok_or("message too short")?;
    let algorithm = key.algorithm.name();
    let mut digest = msg.to_vec();
    digest.extend(variables(&key.name, &algorithm, time, FUDGE, 0, &[])?);
    let mac = key.algorithm.mac(&key.secret, &digest);
    let mut rdata = algorithm.canonical_wire().map_err(|e| e.to_string())?;
    rdata.extend_from_slice(&u48(time));
    rdata.extend_from_slice(&FUDGE.to_be_bytes());
    rdata.extend_from_slice(&len16(mac.len())?);
    rdata.extend_from_slice(&mac);
    rdata.extend_from_slice(&id.to_be_bytes());
    rdata.extend_from_slice(&0u16.to_be_bytes());
    rdata.extend_from_slice(&0u16.to_be_bytes());
    let mut out = msg.to_vec();
    out.extend(key.name.canonical_wire().map_err(|e| e.to_string())?);
    out.extend_from_slice(&dns::TYPE_TSIG.to_be_bytes());
    out.extend_from_slice(&dns::CLASS_ANY.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&len16(rdata.len())?);
    out.extend(rdata);
    set_u16(&mut out, 10, ar.checked_add(1).ok_or("too many records")?).ok_or("message too short")?;
    Ok((out, mac))
}

/// A name written without compression, as TSIG's algorithm name must be, and where it ends.
fn plain_name(data: &[u8]) -> Option<(Name, usize)> {
    let mut labels = Vec::new();
    let mut pos = 0usize;
    loop {
        let len = usize::from(*data.get(pos)?);
        pos += 1;
        if len == 0 {
            return Some((Name::from_labels(labels), pos));
        }
        if len > 63 || pos + len > 255 {
            return None;
        }
        labels.push(String::from_utf8_lossy(data.get(pos..pos + len)?).into_owned());
        pos += len;
    }
}

/// The TSIG record ending `msg`, and the message as its MAC covers it: without that record,
/// ARCOUNT one less, the original id.
pub fn split_signed(msg: &[u8]) -> Result<(Vec<u8>, Signature), String> {
    let at = dns::last_record_start(msg).map_err(|e| format!("cannot read the answer: {e}"))?;
    let decoded = Message::decode(msg).map_err(|e| format!("cannot read the answer: {e}"))?;
    let last = decoded.additionals.last().ok_or("the message is not signed")?;
    let RData::Other { rtype: dns::TYPE_TSIG, data } = &last.data else {
        return Err("the message is not signed".to_owned());
    };
    let bad = || "malformed TSIG record".to_owned();
    let (algorithm, mut p) = plain_name(data).ok_or_else(bad)?;
    let time = data.get(p..p + 6).ok_or_else(bad)?.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b));
    p += 6;
    let fudge = get_u16(data, p).ok_or_else(bad)?;
    let mac_len = usize::from(get_u16(data, p + 2).ok_or_else(bad)?);
    p += 4;
    let mac = data.get(p..p + mac_len).ok_or_else(bad)?.to_vec();
    p += mac_len;
    let original_id = get_u16(data, p).ok_or_else(bad)?;
    let error = get_u16(data, p + 2).ok_or_else(bad)?;
    let other_len = usize::from(get_u16(data, p + 4).ok_or_else(bad)?);
    p += 6;
    let other = data.get(p..p + other_len).ok_or_else(bad)?.to_vec();
    if p + other_len != data.len() {
        return Err(bad());
    }
    let mut unsigned = msg.get(..at).ok_or_else(bad)?.to_vec();
    let ar = get_u16(&unsigned, 10).and_then(|n| n.checked_sub(1)).ok_or_else(bad)?;
    set_u16(&mut unsigned, 10, ar).ok_or_else(bad)?;
    set_u16(&mut unsigned, 0, original_id).ok_or_else(bad)?;
    Ok((unsigned, Signature { key: last.name.clone(), algorithm, time, fudge, mac, original_id, error, other }))
}

/// Check the signature of a signed message: a request (`request_mac` `None`), or the answer to
/// one, whose MAC also covers the request's MAC (RFC 8945 §4.3.1). `now` must lie within the
/// signature's fudge of its time.
pub fn verify(msg: &[u8], key: &Key, request_mac: Option<&[u8]>, now: u64) -> Result<Signature, String> {
    let (unsigned, sig) = split_signed(msg)?;
    if sig.error != 0 {
        return Err(format!("the name server refused the signature: {}", dns::rcode_name(sig.error)));
    }
    if sig.key != key.name || sig.algorithm != key.algorithm.name() {
        return Err(format!("signed with another key ({} {})", sig.key, sig.algorithm));
    }
    let mut digest = Vec::new();
    if let Some(rm) = request_mac {
        digest.extend_from_slice(&len16(rm.len())?);
        digest.extend_from_slice(rm);
    }
    digest.extend(unsigned);
    digest.extend(variables(&sig.key, &sig.algorithm, sig.time, sig.fudge, sig.error, &sig.other)?);
    let want = key.algorithm.mac(&key.secret, &digest);
    if !crate::util::ct_eq(&want, &sig.mac) {
        return Err("the signature does not verify (BADSIG)".to_owned());
    }
    if now.abs_diff(sig.time) > u64::from(sig.fudge) {
        return Err("the signature is too old or from the future (BADTIME)".to_owned());
    }
    Ok(sig)
}

// ------------------------------------------------------------------------------ records

/// This instance's DNS-SD records under `zone` (the root name: relative to a zone file's origin).
pub fn records(a: &Advert, zone: &Name, v4: &[Ipv4Addr], v6: &[Ipv6Addr], ttl: u32) -> Vec<Record> {
    let service = Name::parse(SERVICE_TYPE).join(zone);
    let instance = Name::child(&a.instance_label(), &service);
    let host = Name::from_labels([a.host_label()]).join(zone);
    let mut out = vec![
        Record::new(service, ttl, false, RData::Ptr(instance.clone())),
        Record::new(instance.clone(), ttl, false, RData::Srv { priority: 0, weight: 0, port: a.port, target: host.clone() }),
        Record::new(instance, ttl, false, RData::Txt(a.txt())),
    ];
    out.extend(v4.iter().map(|ip| Record::new(host.clone(), ttl, false, RData::A(*ip))));
    out.extend(v6.iter().map(|ip| Record::new(host.clone(), ttl, false, RData::Aaaa(*ip))));
    out
}

/// Records in zone-file syntax, one per line (names absolute when `absolute`).
pub fn zone_lines(records: &[Record], absolute: bool) -> Vec<String> {
    records
        .iter()
        .filter_map(|r| {
            let (kind, data) = match &r.data {
                RData::Ptr(t) => ("PTR", t.to_zone(absolute)),
                RData::Srv { priority, weight, port, target } => ("SRV", format!("{priority} {weight} {port} {}", target.to_zone(absolute))),
                RData::Txt(strings) => ("TXT", strings.iter().map(|s| dns::zone_txt_string(s)).collect::<Vec<_>>().join(" ")),
                RData::A(ip) => ("A", ip.to_string()),
                RData::Aaaa(ip) => ("AAAA", ip.to_string()),
                RData::Other { .. } => return None,
            };
            Some(format!("{} {} IN {kind} {data}", r.name.to_zone(absolute), r.ttl))
        })
        .collect()
}

fn update_message(id: u16, zone: &Name, updates: Vec<Record>) -> Message {
    Message {
        id,
        flags: dns::OPCODE_UPDATE,
        questions: vec![Question { name: zone.clone(), qtype: dns::TYPE_SOA, qclass: dns::CLASS_IN, unicast: false }],
        answers: Vec::new(),
        authorities: updates,
        additionals: Vec::new(),
    }
}

/// "Delete every record of `name`" (RFC 2136 §2.5.3).
fn delete_name(name: &Name) -> Record {
    Record { name: name.clone(), class: dns::CLASS_ANY, cache_flush: false, ttl: 0, data: RData::Other { rtype: dns::TYPE_ANY, data: Vec::new() } }
}

/// "Delete this record" (RFC 2136 §2.5.4).
fn delete_record(r: &Record) -> Record {
    Record { name: r.name.clone(), class: dns::CLASS_NONE, cache_flush: false, ttl: 0, data: r.data.clone() }
}

/// The owners of `records` other than the shared service name.
fn own_names(records: &[Record]) -> Vec<Name> {
    let mut names: Vec<Name> = Vec::new();
    for r in records.iter().filter(|r| r.rtype() != dns::TYPE_PTR) {
        if !names.contains(&r.name) {
            names.push(r.name.clone());
        }
    }
    names
}

/// The update that makes the zone hold `records`, replacing `before` (what an earlier update put
/// there, if it differs).
pub fn replace(id: u16, zone: &Name, records: &[Record], before: &[Record]) -> Message {
    let mut updates: Vec<Record> = Vec::new();
    for r in before.iter().filter(|r| r.rtype() == dns::TYPE_PTR && !records.contains(r)) {
        updates.push(delete_record(r));
    }
    let mut names = own_names(before);
    for n in own_names(records) {
        if !names.contains(&n) {
            names.push(n);
        }
    }
    updates.extend(names.iter().map(delete_name));
    updates.extend(records.iter().cloned());
    update_message(id, zone, updates)
}

/// The update that takes `records` out of the zone.
pub fn withdraw(id: u16, zone: &Name, records: &[Record]) -> Message {
    let mut updates: Vec<Record> = records.iter().filter(|r| r.rtype() == dns::TYPE_PTR).map(delete_record).collect();
    updates.extend(own_names(records).iter().map(delete_name));
    update_message(id, zone, updates)
}

/// Send a signed update and check the signed answer.
pub async fn send(server: SocketAddr, key: &Key, msg: &Message) -> Result<(), String> {
    let bytes = msg.encode().map_err(|e| format!("cannot encode the update: {e}"))?;
    let now = u64::try_from(crate::util::now_unix()).unwrap_or(0);
    let (signed, mac) = sign(&bytes, key, now)?;
    let answer = super::unicast::exchange(server, &signed, msg.id, Duration::from_secs(3)).await?;
    let m = Message::decode(&answer).map_err(|e| format!("cannot read the answer of {server}: {e}"))?;
    if !m.is_response() || m.opcode() != dns::OPCODE_UPDATE {
        return Err(format!("{server} did not answer the update"));
    }
    let signed_answer = m.additionals.last().is_some_and(|r| r.rtype() == dns::TYPE_TSIG);
    if signed_answer {
        // The answer's signature first: an unverified "refused" is as suspect as an unverified "ok".
        verify(&answer, key, Some(&mac), now)?;
    }
    if m.rcode() != dns::RCODE_NOERROR {
        return Err(format!("{server} refused the update: {}", dns::rcode_name(m.rcode())));
    }
    if !signed_answer {
        return Err(format!("{server} answered without a signature"));
    }
    Ok(())
}

/// The addresses to publish: this machine's LAN IPv4 addresses, the one on the route to the
/// name server first; IPv6 ones (not link-local) when the HTTP server listens on IPv6.
fn addresses(d: &Discovery, toward: Option<IpAddr>) -> (Vec<Ipv4Addr>, Vec<Ipv6Addr>) {
    let mut v4: Vec<Ipv4Addr> = Vec::new();
    let mut v6: Vec<Ipv6Addr> = Vec::new();
    if let Some(IpAddr::V4(ip)) = toward.and_then(super::local_address_for)
        && !ip.is_loopback()
    {
        v4.push(ip);
    }
    for i in super::interfaces(&d.settings.interfaces) {
        for (ip, _) in i.v4 {
            if !ip.is_link_local() && !v4.contains(&ip) {
                v4.push(ip);
            }
        }
        if d.ipv6 {
            for ip in i.v6 {
                if !ip.is_unicast_link_local() && !v6.contains(&ip) {
                    v6.push(ip);
                }
            }
        }
    }
    v4.truncate(8);
    v6.truncate(8);
    (v4, v6)
}

/// The zone records go to in `GET /api/discovery`'s zone-file lines: the published zone, else
/// the first configured domain; `None`: names relative to the zone's origin.
pub fn record_zone(d: &Discovery) -> Option<Name> {
    d.settings
        .publish
        .as_ref()
        .and_then(|p| super::unicast::browsable(&p.zone))
        .or_else(|| d.settings.domains.iter().find_map(|s| super::unicast::browsable(s)))
}

/// The zone-file lines an admin can add to publish this instance by hand.
pub fn manual_records(d: &Discovery) -> Vec<String> {
    let zone = record_zone(d);
    let (v4, v6) = addresses(d, None);
    let advert = d.advert.lock().clone();
    zone_lines(&records(&advert, zone.as_ref().unwrap_or(&Name::default()), &v4, &v6, RECORD_TTL), zone.is_some())
}

/// `host[:port]`, an address or `[v6]:port` as `host:port` (port 53 when none is given).
fn server_target(s: &str) -> String {
    let s = s.trim();
    if s.parse::<SocketAddr>().is_ok() {
        return s.to_owned();
    }
    if let Ok(ip) = s.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>() {
        return SocketAddr::new(ip, 53).to_string();
    }
    if s.rsplit_once(':').is_some_and(|(_, port)| port.parse::<u16>().is_ok()) {
        return s.to_owned();
    }
    format!("{s}:53")
}

fn set_status(d: &Discovery, result: Result<(), String>) {
    let mut h = d.health.lock();
    h.publish.at = Some(crate::util::now_unix());
    match result {
        Ok(()) => {
            h.publish.ok = Some(true);
            h.publish.error = None;
        }
        Err(e) => {
            h.publish.ok = Some(false);
            h.publish.error = Some(e);
        }
    }
}

/// Keep this instance published until `shutdown`, then withdraw it.
pub async fn run(d: &Arc<Discovery>, shutdown: &Shutdown) {
    let Some(p) = d.settings.publish.clone() else { return };
    if !d.announcing() {
        d.health.lock().publish.error = Some("announcing is turned off ([discovery] announce = false)".to_owned());
        return;
    }
    let Some(zone) = super::unicast::browsable(&p.zone) else {
        set_status(d, Err(format!("[discovery.publish] zone {:?} is not a domain name", p.zone)));
        return;
    };
    // What the zone holds from us, where, with which key; when it was last pushed.
    let mut published: Option<(Vec<Record>, SocketAddr, Key)> = None;
    let mut pushed: Option<Instant> = None;
    loop {
        let round = async {
            let key = Key::load(&p)?;
            let server = crate::rt::resolve(&server_target(&p.server)).await.map_err(|e| format!("name server {}: {e:#}", p.server))?;
            let advert = d.advert.lock().clone();
            let (v4, v6) = addresses(d, Some(server.ip()));
            let records = records(&advert, &zone, &v4, &v6, RECORD_TTL);
            let unchanged = published.as_ref().is_some_and(|(r, s, _)| *r == records && *s == server);
            if unchanged && pushed.is_some_and(|t| t.elapsed() < REFRESH_EVERY) {
                return Ok(());
            }
            let before = published.as_ref().map(|(r, _, _)| r.clone()).unwrap_or_default();
            send(server, &key, &replace(rand::random(), &zone, &records, &before)).await?;
            if !unchanged {
                tracing::info!(zone = %zone, server = %server, "discovery: published to DNS");
            }
            published = Some((records, server, key));
            pushed = Some(Instant::now());
            Ok(())
        };
        let mut finished = None;
        let ran = super::until_stopped(shutdown, async { finished = Some(round.await) }).await;
        if let Some(result) = finished {
            if let Err(e) = &result {
                tracing::warn!(error = %e, "discovery: publishing to DNS failed");
            }
            set_status(d, result);
        }
        if !ran || !shutdown.sleep(CHECK_EVERY).await {
            break;
        }
    }
    if let Some((records, server, key)) = published {
        let msg = withdraw(rand::random(), &zone, &records);
        match crate::rt::timeout(Duration::from_secs(2), send(server, &key, &msg)).await {
            Ok(Ok(())) => tracing::info!(zone = %zone, "discovery: withdrawn from DNS"),
            Ok(Err(e)) => tracing::warn!(error = %e, "discovery: withdrawing from DNS failed"),
            Err(_) => tracing::warn!("discovery: withdrawing from DNS timed out"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> Key {
        Key { name: Name::parse("kiln-key."), algorithm: Algorithm::HmacSha256, secret: b"0123456789abcdef0123456789abcdef".to_vec() }
    }

    fn advert() -> Advert {
        let mut a = Advert::new("1a2b3c4d5e6f70819293a4b5c6d7e8f9".into(), super::super::Role::Server, 7070, 7071);
        a.name = "box".into();
        a.ca = Some("0123456789abcdef".into());
        a.os = "linux".into();
        a.arch = "x86_64".into();
        a.version = "0.1.0".into();
        a
    }

    /// The MAC computed field by field, independently of `sign`'s code: HMAC-SHA256 (through
    /// `util::hmac_hex`) over the message and the TSIG variables of RFC 8945 §4.3.3.
    fn independent_mac(msg: &[u8], secret: &[u8], time: u64) -> String {
        let mut d = msg.to_vec();
        d.extend_from_slice(b"\x08kiln-key\x00"); // key name, canonical
        d.extend_from_slice(&[0x00, 0xFF]); // class ANY
        d.extend_from_slice(&[0, 0, 0, 0]); // TTL 0
        d.extend_from_slice(b"\x0bhmac-sha256\x00"); // algorithm name, canonical
        d.extend_from_slice(time.to_be_bytes().get(2..).unwrap_or_default()); // time signed, 48 bits
        d.extend_from_slice(&[0x01, 0x2C]); // fudge 300
        d.extend_from_slice(&[0, 0, 0, 0]); // error 0, other length 0
        crate::util::hmac_hex(secret, &d)
    }

    #[test]
    fn signatures_match_an_independent_computation_and_verify() -> Result<(), String> {
        let zone = Name::parse("example.com");
        let a = advert();
        let records = records(&a, &zone, &[Ipv4Addr::new(192, 168, 1, 10)], &[], RECORD_TTL);
        let msg = replace(0x1234, &zone, &records, &[]).encode().map_err(|e| e.to_string())?;
        let time = 1_760_000_000;
        let (signed, mac) = sign(&msg, &key(), time)?;
        assert_eq!(hex::encode(&mac), independent_mac(&msg, &key().secret, time));
        // The signed message decodes, ends with the TSIG record, and verifies as a request.
        let m = Message::decode(&signed).map_err(|e| e.to_string())?;
        assert_eq!(m.additionals.last().map(Record::rtype), Some(dns::TYPE_TSIG));
        let sig = verify(&signed, &key(), None, time + 10)?;
        assert_eq!((sig.original_id, sig.fudge, sig.mac.clone()), (0x1234, FUDGE, mac.clone()));
        // A changed byte, another secret, or a stale time fail.
        let mut tampered = signed.clone();
        if let Some(b) = tampered.get_mut(20) {
            *b ^= 1;
        }
        assert!(verify(&tampered, &key(), None, time).is_err());
        let other = Key { secret: b"another secret".to_vec(), ..key() };
        assert!(verify(&signed, &other, None, time).is_err_and(|e| e.contains("BADSIG")));
        assert!(verify(&signed, &key(), None, time + 301).is_err_and(|e| e.contains("BADTIME")));
        // A response's MAC covers the request's MAC first.
        let mut answer = update_message(0x1234, &zone, Vec::new());
        answer.flags |= dns::FLAG_RESPONSE;
        let answer = answer.encode().map_err(|e| e.to_string())?;
        let mut digest = (u16::try_from(mac.len()).unwrap_or(0)).to_be_bytes().to_vec();
        digest.extend_from_slice(&mac);
        digest.extend_from_slice(&answer);
        let (signed_answer, _) = sign(&answer, &key(), time)?;
        // `sign` signs as a request; re-sign by hand as the response to `mac`.
        let (unsigned, mut sig) = split_signed(&signed_answer)?;
        assert_eq!(unsigned, answer);
        digest.extend(variables(&sig.key, &sig.algorithm, sig.time, sig.fudge, 0, &[])?);
        sig.mac = Algorithm::HmacSha256.mac(&key().secret, &digest);
        let resigned = {
            let mut rdata = sig.algorithm.canonical_wire().map_err(|e| e.to_string())?;
            rdata.extend_from_slice(&u48(sig.time));
            rdata.extend_from_slice(&sig.fudge.to_be_bytes());
            rdata.extend_from_slice(&len16(sig.mac.len())?);
            rdata.extend_from_slice(&sig.mac);
            rdata.extend_from_slice(&[0x12, 0x34, 0, 0, 0, 0]);
            let mut m = Message::decode(&signed_answer).map_err(|e| e.to_string())?;
            if let Some(last) = m.additionals.last_mut() {
                last.data = RData::Other { rtype: dns::TYPE_TSIG, data: rdata };
            }
            m.encode().map_err(|e| e.to_string())?
        };
        assert!(verify(&resigned, &key(), Some(&mac), time).is_ok());
        assert!(verify(&resigned, &key(), None, time).is_err());
        Ok(())
    }

    /// A DNS UPDATE that BIND 9's `nsupdate` signed (key `kiln-test.`, hmac-sha256, the secret
    /// below), captured off the wire: kiln's verification computes the same MAC as BIND.
    #[test]
    fn a_signature_made_by_bind_verifies() -> Result<(), String> {
        let secret = base64_secret(BIND_SECRET)?;
        let k = Key { name: Name::parse("kiln-test"), algorithm: Algorithm::HmacSha256, secret };
        let msg = hex::decode(BIND_UPDATE).map_err(|e| e.to_string())?;
        let (_, sig) = split_signed(&msg)?;
        let verified = verify(&msg, &k, None, sig.time)?;
        assert_eq!(verified.algorithm, Name::parse("hmac-sha256"));
        // (Not a longer secret: HMAC pads short keys with zeros, so `secret + [0]` is the same key.)
        let mut wrong = k.clone();
        if let Some(b) = wrong.secret.first_mut() {
            *b ^= 0x80;
        }
        assert!(verify(&msg, &wrong, None, sig.time).is_err_and(|e| e.contains("BADSIG")));
        Ok(())
    }

    fn base64_secret(s: &str) -> Result<Vec<u8>, String> {
        parse_secret(s).map(|(k, _)| k).ok_or_else(|| "bad secret".to_owned())
    }

    const BIND_SECRET: &str = "key \"kiln-test\" {\n\talgorithm hmac-sha256;\n\tsecret \"efr39UhMZMKKIO2D8O5mdeHUoIkHtqODds8luSxrCSw=\";\n};\n";
    const BIND_UPDATE: &str = concat!(
        "92d228000001000000020001076578616d706c6503636f6d0000060001055f6b696c6e045f746370c00c000c00010000",
        "012c00201d6b696c6e20736572766572206f6e20626f782028316132623363346429c01d0d6b696c6e2d316132623363",
        "3464c00c000100010000012c0004c0a8010a096b696c6e2d746573740000fa00ff00000000003d0b686d61632d736861",
        "3235360000006ad2ff3f012c0020fccc9d208709cb5270ac28cf6ee575055de9af27d3e1267c4bcd99efbac5fbd392d2",
        "00000000",
    );

    #[test]
    fn key_files() {
        let plain = parse_secret("  c2VjcmV0LXNlY3JldA==\n");
        assert_eq!(plain, Some((b"secret-secret".to_vec(), None)));
        let bind = parse_secret("key \"kiln\" {\n\talgorithm hmac-sha512;\n\tsecret \"c2Vj\ncmV0\";\n};\n");
        assert_eq!(bind, Some((b"secret".to_vec(), Some("hmac-sha512".to_owned()))));
        assert_eq!(parse_secret("not base64!"), None);
        assert_eq!(Algorithm::parse("HMAC-SHA384."), Some(Algorithm::HmacSha384));
        assert_eq!(Algorithm::parse("hmac-md5"), None);
        assert_eq!(Algorithm::parse(""), Some(Algorithm::HmacSha256));
        assert_eq!(server_target("192.168.1.1"), "192.168.1.1:53");
        assert_eq!(server_target("ns1.example.com"), "ns1.example.com:53");
        assert_eq!(server_target("ns1.example.com:5300"), "ns1.example.com:5300");
        assert_eq!(server_target("fd00::53"), "[fd00::53]:53");
        assert_eq!(server_target("[fd00::53]:5300"), "[fd00::53]:5300");
    }

    #[test]
    fn updates_replace_and_withdraw() -> Result<(), dns::Error> {
        let zone = Name::parse("example.com");
        let a = advert();
        let now = records(&a, &zone, &[Ipv4Addr::new(192, 168, 1, 10)], &[], RECORD_TTL);
        let m = Message::decode(&replace(7, &zone, &now, &[]).encode()?)?;
        assert_eq!(m.opcode(), dns::OPCODE_UPDATE);
        assert_eq!(m.questions.first().map(|q| (q.name.clone(), q.qtype)), Some((zone.clone(), dns::TYPE_SOA)));
        // Delete the instance's and the host's names, then add the set (RFC 2136 runs them in order).
        let kinds: Vec<(u16, u16)> = m.authorities.iter().map(|r| (r.class, r.rtype())).collect();
        assert_eq!(
            kinds,
            vec![
                (dns::CLASS_ANY, dns::TYPE_ANY),
                (dns::CLASS_ANY, dns::TYPE_ANY),
                (dns::CLASS_IN, dns::TYPE_PTR),
                (dns::CLASS_IN, dns::TYPE_SRV),
                (dns::CLASS_IN, dns::TYPE_TXT),
                (dns::CLASS_IN, dns::TYPE_A),
            ]
        );
        // A renamed instance: its old PTR goes, specifically (class NONE).
        let mut renamed = a.clone();
        renamed.name = "box2".into();
        let later = records(&renamed, &zone, &[Ipv4Addr::new(192, 168, 1, 10)], &[], RECORD_TTL);
        let m = replace(8, &zone, &later, &now);
        assert_eq!(m.authorities.first().map(|r| (r.class, r.ttl, r.data.clone())), now.first().map(|r| (dns::CLASS_NONE, 0, r.data.clone())));
        let w = withdraw(9, &zone, &later);
        assert!(w.authorities.iter().all(|r| r.ttl == 0 && r.class != dns::CLASS_IN));
        assert_eq!(w.authorities.len(), 3);
        Ok(())
    }

    #[test]
    fn zone_file_lines() {
        let a = advert();
        let lines = zone_lines(&records(&a, &Name::default(), &[Ipv4Addr::new(192, 168, 1, 10)], &[], 300), false);
        assert_eq!(lines.first().map(String::as_str), Some(r"_kiln._tcp 300 IN PTR kiln\032server\032on\032box\032\(1a2b3c4d\)._kiln._tcp"));
        assert_eq!(lines.get(1).map(String::as_str), Some(r"kiln\032server\032on\032box\032\(1a2b3c4d\)._kiln._tcp 300 IN SRV 0 0 7070 kiln-1a2b3c4d"));
        assert!(lines.get(2).is_some_and(|l| l.contains(r#"IN TXT "id=1a2b3c4d5e6f70819293a4b5c6d7e8f9" "name=box" "role=server""#)));
        assert_eq!(lines.get(3).map(String::as_str), Some("kiln-1a2b3c4d 300 IN A 192.168.1.10"));
        let absolute = zone_lines(&records(&a, &Name::parse("home.example"), &[], &[], 300), true);
        assert_eq!(absolute.first().map(String::as_str), Some(r"_kiln._tcp.home.example. 300 IN PTR kiln\032server\032on\032box\032\(1a2b3c4d\)._kiln._tcp.home.example."));
    }
}
