//! The HTTP/3 door in the server (goal 4 step 6, plan 01a1a832eb597110a6dcfda227981604): with
//! `[server] h3_addr` set, a WebTransport door on that UDP address (kiln-http's `h3`), and off
//! without it: no setting, no socket. It shares no port (not the runner QUIC listener's, whose
//! mutual TLS browsers can't meet, and not the front door's, which carries no UDP).
//!
//! **Certificates.** Chrome takes no privately rooted certificate for QUIC, so unless the person
//! names a publicly trusted one (`h3_cert` and `h3_key`), the door serves self-signed ECDSA P-256
//! certificates a page pins by their SHA-256 (WebTransport's `serverCertificateHashes`, which
//! takes only a certificate valid for at most two weeks). They rotate with an overlap
//! ([`Rotation`]): each is served for a week and valid for 13 days, and a page is given the
//! hashes of the current one and the next, so a certificate served is always in a list a page
//! fetched within its ticket's life.
//!
//! **Who.** WebTransport sends no cookies, so a page first asks `POST /api/me/door` (its
//! `kiln_session` cookie authenticating it) for the door's URL, the hashes and a single-use
//! ticket that lasts 30 seconds; the CONNECT carries the ticket in its path, and the door admits
//! it only with a ticket not yet used, for the principal the path names, from one of the server's
//! own origins ([`admit`]). Goal 2's ticket rules (32 random bytes, once, 30 seconds).
//!
//! **Lockless** (the API runtime's rule, `scripts/locks/hot.toml`): nothing here is shared under a
//! lock. What the API and the door both read ([`Published`]: the URL, the hashes, the origins, the
//! tickets' key) is an immutable copy swapped in whole when the certificate rotates
//! (`crate::rcu::Rcu`). A ticket carries its own proof ([`TicketKey`]: 32 random bytes, when it was
//! issued and an HMAC-SHA256 over them and its principal), so minting one writes nothing; using
//! one up is the door's to record, and the door's driver is one task, which owns that record
//! ([`Used`]) alone.
//!
//! Goal 7 owns the rest of the door's TLS (chain size, compression, resumption policy,
//! post-quantum, 0-RTT); this turns session tickets on (one a handshake) and early data off.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use anyhow::Context as _;
use base64::Engine as _;
use kiln_http::h3::conn::{Admission, Connect, Event};
use kiln_http::h3::door::{ConnId, Door, DoorConfig};
use kiln_http::h3::driver::{Driver, Host};
use kiln_http::h3::{Admit, Read};
use serde_json::{Value, json};
use sha2::Digest as _;

use crate::hub::Hub;
use crate::rcu::Rcu;

/// A ticket's life (as goal 2's: 30 seconds).
pub const TICKET_LIFE_MS: u64 = 30_000;

// ------------------------------------------------------------------------------- rotation

/// The hash-pinned certificates' schedule: certificate `k` is served from `k * period` for one
/// period, valid from an hour before that (clock skew) for `validity`, under 14 days whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rotation {
    pub period_s: i64,
    pub validity_s: i64,
    /// How far before its period a certificate is valid.
    pub skew_s: i64,
}

impl Rotation {
    pub const DOOR: Rotation = Rotation { period_s: 7 * 86_400, validity_s: 13 * 86_400, skew_s: 3_600 };

    /// The certificate served at `now` (unix seconds).
    pub fn epoch(&self, now: i64) -> i64 {
        now.div_euclid(self.period_s)
    }

    /// Certificate `k`'s validity, unix seconds.
    pub fn validity(&self, k: i64) -> (i64, i64) {
        let from = k * self.period_s - self.skew_s;
        (from, from + self.validity_s)
    }
}

/// A door certificate: DER, its key (PKCS#8 DER), its validity and its SHA-256.
#[derive(Clone)]
pub struct DoorCert {
    pub der: Vec<u8>,
    pub key: Vec<u8>,
    pub not_before: i64,
    pub not_after: i64,
    pub sha256: [u8; 32],
}

impl std::fmt::Debug for DoorCert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DoorCert").field("not_before", &self.not_before).field("not_after", &self.not_after).field("key", &"<redacted>").finish()
    }
}

/// Issues certificate `k` of `rotation` for `names`: self-signed ECDSA P-256 (rcgen's default).
pub fn issue(rotation: &Rotation, k: i64, names: &[String]) -> anyhow::Result<DoorCert> {
    let (from, to) = rotation.validity(k);
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).context("a P-256 key")?;
    let mut p = rcgen::CertificateParams::new(names.to_vec()).context("the door's names")?;
    p.not_before = time::OffsetDateTime::from_unix_timestamp(from).context("notBefore")?;
    p.not_after = time::OffsetDateTime::from_unix_timestamp(to).context("notAfter")?;
    p.distinguished_name.push(rcgen::DnType::CommonName, "kiln WebTransport door");
    let cert = p.self_signed(&key).context("self-sign the door's certificate")?;
    let der = cert.der().to_vec();
    let sha256: [u8; 32] = sha2::Sha256::digest(&der).into();
    Ok(DoorCert { der, key: key.serialize_der(), not_before: from, not_after: to, sha256 })
}

/// The current certificate and the next, kept as time passes (time passed in).
pub struct Certs {
    pub rotation: Rotation,
    pub current: (i64, DoorCert),
    pub next: (i64, DoorCert),
}

impl Certs {
    pub fn new(rotation: Rotation, now: i64, mut issue: impl FnMut(i64) -> anyhow::Result<DoorCert>) -> anyhow::Result<Certs> {
        let k = rotation.epoch(now);
        Ok(Certs { rotation, current: (k, issue(k)?), next: (k + 1, issue(k + 1)?) })
    }

    /// Moves to `now`'s certificate, issuing what's needed: whether the one served changed.
    pub fn at(&mut self, now: i64, mut issue: impl FnMut(i64) -> anyhow::Result<DoorCert>) -> anyhow::Result<bool> {
        let k = self.rotation.epoch(now);
        if k == self.current.0 {
            return Ok(false);
        }
        if k == self.next.0 {
            let next = (k + 1, issue(k + 1)?);
            self.current = std::mem::replace(&mut self.next, next);
        } else {
            self.current = (k, issue(k)?);
            self.next = (k + 1, issue(k + 1)?);
        }
        Ok(true)
    }

    /// The hashes a page pins: the current certificate's and the next's.
    pub fn hashes(&self) -> [[u8; 32]; 2] {
        [self.current.1.sha256, self.next.1.sha256]
    }
}

// ------------------------------------------------------------------------------- tickets

/// Why a ticket was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Not one this server made (or made for someone else): its proof doesn't hold.
    Unknown,
    Used,
    Expired,
}

/// The bytes of a ticket: 32 random, when it was issued (ms, big-endian), and 16 of its proof.
const TICKET_LEN: usize = 32 + 8 + 16;

/// The key the server's tickets are proved with: 32 random bytes made when the door starts. A
/// ticket is `nonce (32 random bytes) | issued at (ms) | HMAC-SHA256(key, nonce | issued | principal)`
/// cut to 16 bytes, so whoever holds the key can tell, with nothing written anywhere, that the
/// server issued it, when, and for whom.
#[derive(Clone)]
pub struct TicketKey(aws_lc_rs::hmac::Key);

impl std::fmt::Debug for TicketKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TicketKey(<redacted>)")
    }
}

impl TicketKey {
    pub fn new(secret: [u8; 32]) -> Self {
        Self(aws_lc_rs::hmac::Key::new(aws_lc_rs::hmac::HMAC_SHA256, &secret))
    }

    fn tag(&self, nonce: &[u8], issued: &[u8], principal: &str) -> [u8; 16] {
        let mut ctx = aws_lc_rs::hmac::Context::with_key(&self.0);
        ctx.update(nonce);
        ctx.update(issued);
        ctx.update(principal.as_bytes());
        let full = ctx.sign();
        let mut out = [0u8; 16];
        if let Some(t) = full.as_ref().get(..16) {
            out.copy_from_slice(t);
        }
        out
    }

    /// A ticket of the caller's 32 random bytes for `principal`, issued at `now_ms`.
    pub fn mint(&self, nonce: [u8; 32], principal: &str, now_ms: u64) -> Vec<u8> {
        let issued = now_ms.to_be_bytes();
        let mut t = Vec::with_capacity(TICKET_LEN);
        t.extend_from_slice(&nonce);
        t.extend_from_slice(&issued);
        t.extend_from_slice(&self.tag(&nonce, &issued, principal));
        t
    }

    /// Checks a ticket's proof for `principal` and its life at `now_ms`: its nonce and when it was
    /// issued, for the one-use record.
    pub fn check(&self, ticket: &[u8], principal: &str, now_ms: u64) -> Result<([u8; 32], u64), Refusal> {
        if ticket.len() != TICKET_LEN {
            return Err(Refusal::Unknown);
        }
        let (nonce, rest) = ticket.split_at_checked(32).ok_or(Refusal::Unknown)?;
        let (issued, tag) = rest.split_at_checked(8).ok_or(Refusal::Unknown)?;
        let want = self.tag(nonce, issued, principal);
        // Constant time: a guess learns nothing from how long the comparison took.
        if aws_lc_rs::constant_time::verify_slices_are_equal(&want, tag).is_err() {
            return Err(Refusal::Unknown);
        }
        let at = u64::from_be_bytes(issued.try_into().map_err(|_| Refusal::Unknown)?);
        // Expired, or issued in the future (past a second's skew).
        if now_ms.saturating_sub(at) > TICKET_LIFE_MS || at > now_ms.saturating_add(1_000) {
            return Err(Refusal::Expired);
        }
        let mut n = [0u8; 32];
        n.copy_from_slice(nonce);
        Ok((n, at))
    }
}

/// The tickets used, owned by the one task that admits sessions (the door's driver): each nonce
/// until its ticket's life is past twice over, when the ticket is refused as expired anyway.
#[derive(Debug, Default)]
pub struct Used {
    seen: BTreeMap<[u8; 32], u64>,
}

impl Used {
    /// Uses a ticket up: `Err(Used)` when it was before.
    pub fn take(&mut self, nonce: [u8; 32], issued: u64, now_ms: u64) -> Result<(), Refusal> {
        self.seen.retain(|_, at| now_ms.saturating_sub(*at) <= 2 * TICKET_LIFE_MS);
        if self.seen.contains_key(&nonce) {
            return Err(Refusal::Used);
        }
        self.seen.insert(nonce, issued);
        Ok(())
    }

    /// Tickets held.
    pub fn held(&self) -> usize {
        self.seen.len()
    }
}

// ------------------------------------------------------------------------------- admission

/// The paths the door serves: `/wt` (the browser protocol's, goal 5's to serve) and `/wt/echo`
/// (echoes, for the lab's browser checks).
pub const PATHS: [&str; 2] = ["/wt", "/wt/echo"];

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// Decides a CONNECT: an origin of the server's own, a path it serves, a ticket of `key`'s, within
/// its life, for the principal the path names (`?ticket=…&u=…`), not used before (`used`, the
/// admitting task's own).
pub fn admit(c: &Connect, origins: &[String], key: &TicketKey, used: &mut Used, now_ms: u64) -> Admission {
    let Some(origin) = c.origin.as_deref().and_then(|o| std::str::from_utf8(o).ok()) else { return Admission::Forbidden };
    if !origins.iter().any(|o| o == origin) {
        return Admission::Forbidden;
    }
    let Ok(path) = std::str::from_utf8(&c.path) else { return Admission::NotFound };
    let (route, query) = path.split_once('?').unwrap_or((path, ""));
    if !PATHS.contains(&route) {
        return Admission::NotFound;
    }
    let mut ticket = None;
    let mut user = None;
    for pair in query.split('&') {
        match pair.split_once('=') {
            Some(("ticket", t)) => ticket = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(t).ok(),
            Some(("u", u)) => user = Some(unescape(u)),
            _ => {}
        }
    }
    let (Some(ticket), Some(user)) = (ticket, user) else { return Admission::Forbidden };
    match key.check(&ticket, &user, now_ms).and_then(|(nonce, at)| used.take(nonce, at, now_ms)) {
        Ok(()) => Admission::Accept,
        Err(_) => Admission::Forbidden,
    }
}

/// A query part's `%XX` escapes undone (what [`url_part`] wrote).
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while let Some(&c) = b.get(i) {
        if c == b'%'
            && let Some(h) = b.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(h);
            i += 3;
            continue;
        }
        out.push(c);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A principal as it sits in a URL's query.
fn url_part(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// The origins pages of this server are served from: `public_url`'s, and `https://` with each of
/// `tls_names` (bare, and at `public_url`'s port).
pub fn origins(public_url: &str, tls_names: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let (scheme, rest) = public_url.split_once("://").unwrap_or(("http", public_url));
    let host_port = rest.split('/').next().unwrap_or_default();
    if !host_port.is_empty() {
        out.push(format!("{scheme}://{host_port}"));
    }
    let port = host_port.rsplit_once(':').map(|(_, p)| p).filter(|p| p.chars().all(|c| c.is_ascii_digit()));
    for n in tls_names {
        out.push(format!("https://{n}"));
        if let Some(p) = port {
            out.push(format!("https://{n}:{p}"));
        }
    }
    out.dedup();
    out
}

// ------------------------------------------------------------------------------- the door's state

/// What the HTTP API and the door both read: the URL, the pinned hashes (none for a publicly
/// trusted certificate), the origins, the tickets' key. Immutable; the door swaps a new copy in
/// whole when its certificate rotates.
#[derive(Clone, Debug)]
pub struct Published {
    pub url: String,
    pub hashes: Vec<[u8; 32]>,
    pub origins: Vec<String>,
    pub key: TicketKey,
}

/// The door's published copy, once the door is up: read without a lock (`crate::rcu`).
static DOOR: OnceLock<Rcu<Published>> = OnceLock::new();

fn now_ms() -> u64 {
    u64::try_from(time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(0)
}

/// `POST /api/me/door`: the door's URL, the hashes to pin and a ticket for the caller, or that the
/// door is off.
pub fn info(principal: &str) -> Value {
    match DOOR.get() {
        None => json!({ "on": false }),
        Some(p) => info_of(&p.load(), principal, rand::random(), now_ms()),
    }
}

/// [`info`] over a given copy (the tests' way in): it writes nothing.
pub fn info_of(p: &Published, principal: &str, nonce: [u8; 32], now_ms: u64) -> Value {
    let ticket = p.key.mint(nonce, principal, now_ms);
    json!({
        "on": true,
        "url": format!("{}?ticket={}&u={}", p.url, b64(&ticket), url_part(principal)),
        "hashes": p.hashes.iter().map(|h| base64::engine::general_purpose::STANDARD.encode(h)).collect::<Vec<_>>(),
        "ticket_life_ms": TICKET_LIFE_MS,
    })
}

/// The door's host: tickets decide CONNECTs; `/wt/echo` sessions echo; rotation happens on turns.
struct DoorHost {
    /// The published copy (`DOOR`'s): read on each CONNECT, swapped on rotation.
    published: &'static Rcu<Published>,
    /// The tickets used: this task's alone.
    used: Used,
    certs: Option<(Certs, Vec<String>)>,
    shutdown: crate::rt::Shutdown,
}

impl Admit for DoorHost {
    fn admit(&mut self, c: &Connect) -> Admission {
        let p = self.published.load();
        admit(c, &p.origins, &p.key, &mut self.used, now_ms())
    }
}

impl Host for DoorHost {
    fn event(&mut self, door: &mut Door, conn: ConnId, event: Event, now: Instant) {
        let echo = door.session(conn, now).and_then(|s| s.connect().map(|c| c.path.starts_with(b"/wt/echo"))).unwrap_or(false);
        if !echo {
            return;
        }
        let Some(mut s) = door.session(conn, now) else { return };
        match event {
            Event::Readable(id) => loop {
                match s.read(id, 16 << 10) {
                    Read::Data(b) => {
                        if id & 2 == 0 {
                            let _ = s.write(id, &b);
                        } else if let Some(out) = s.open(false) {
                            let _ = s.write(out, &b);
                            s.finish(out);
                        }
                    }
                    Read::Fin => {
                        if id & 2 == 0 {
                            s.finish(id);
                        }
                        s.forget(id);
                        break;
                    }
                    _ => break,
                }
            },
            Event::Datagram => {
                while let Some(d) = s.recv_datagram() {
                    let _ = s.send_datagram(&d);
                }
            }
            _ => {}
        }
    }

    fn turn(&mut self, door: &mut Door, _now: Instant) -> bool {
        if self.shutdown.is_cancelled() {
            return false;
        }
        if let Some((certs, names)) = &mut self.certs {
            let now = time::OffsetDateTime::now_utc().unix_timestamp();
            match certs.at(now, |k| issue(&certs_rotation(), k, names)) {
                Ok(true) => {
                    match tls_of(&certs.current.1) {
                        Ok(tls) => {
                            if let Err(e) = door.set_tls(tls) {
                                tracing::warn!(error = %e, "the door's next certificate wasn't taken");
                            }
                        }
                        Err(e) => tracing::warn!(error = %format!("{e:#}"), "the door's next certificate"),
                    }
                    let hashes = certs.hashes().to_vec();
                    self.published.update(|p| p.hashes = hashes.clone());
                }
                Ok(false) => {}
                Err(e) => tracing::warn!(error = %format!("{e:#}"), "the door's certificate rotation"),
            }
        }
        true
    }
}

fn certs_rotation() -> Rotation {
    Rotation::DOOR
}

fn tls_of(c: &DoorCert) -> anyhow::Result<hyper_quic::rustls::ServerConfig> {
    use hyper_quic::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    kiln_http::h3::door::tls(vec![CertificateDer::from(c.der.clone())], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(c.key.clone()))).map_err(|e| anyhow::anyhow!("the door's TLS: {e}"))
}

/// The door (a child of the server, with `[server] h3_addr`).
pub async fn run(hub: Arc<Hub>) -> anyhow::Result<()> {
    let s = &hub.cfg.server;
    let addr = s.h3_addr.clone().unwrap_or_default();
    let bind: SocketAddr = addr.trim().parse().with_context(|| format!("h3_addr {addr}"))?;
    let public_host = s.public_url.split_once("://").map_or(s.public_url.as_str(), |(_, r)| r).split('/').next().unwrap_or_default().rsplit_once(':').map_or_else(|| s.public_url.clone(), |(h, _)| h.to_owned());
    let mut names: Vec<String> = s.tls_names.clone();
    names.push(public_host.trim_start_matches('[').trim_end_matches(']').to_owned());
    names.dedup();
    let (tls, certs, hashes) = match (&s.h3_cert, &s.h3_key) {
        (Some(cert), Some(key)) => {
            let chain = std::fs::read_to_string(cert).with_context(|| format!("read {}", cert.display()))?;
            let key = std::fs::read_to_string(key).with_context(|| format!("read {}", key.display()))?;
            let tls = crate::http::tls::ServerTls::from_pem(&chain, &key)?.config_alpn(&[b"h3"])?;
            (tls, None, Vec::new())
        }
        _ => {
            let now = time::OffsetDateTime::now_utc().unix_timestamp();
            let certs = Certs::new(Rotation::DOOR, now, |k| issue(&Rotation::DOOR, k, &names))?;
            let tls = tls_of(&certs.current.1)?;
            let hashes = certs.hashes().to_vec();
            (tls, Some((certs, names.clone())), hashes)
        }
    };
    let door = Door::new(DoorConfig::door(), tls, None).map_err(|e| anyhow::anyhow!("{e}"))?;
    let driver = Driver::bind(door, bind).map_err(|e| anyhow::anyhow!("bind {bind} for the HTTP/3 door: {e}"))?;
    let port = driver.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?.port();
    let host = s.h3_advertise.clone().unwrap_or_else(|| format!("{public_host}:{port}"));
    let fresh = Published { url: format!("https://{host}/wt"), hashes, origins: origins(&s.public_url, &s.tls_names), key: TicketKey::new(rand::random()) };
    let published = DOOR.get_or_init(|| Rcu::new(fresh.clone()));
    // A door started again in the same process (its supervisor restarted it) publishes afresh.
    published.set(fresh);
    tracing::info!(%bind, %host, "the HTTP/3 WebTransport door");
    let mut h = DoorHost { published, used: Used::default(), certs, shutdown: hub.shutdown.clone() };
    driver.run(&mut h).await.map_err(|e| anyhow::anyhow!("the HTTP/3 door: {e}"))
}

#[cfg(test)]
mod tests;
