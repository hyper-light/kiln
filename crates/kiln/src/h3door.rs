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
//! own origins ([`admit`]). The tickets sit behind [`DoorTickets`], to take goal 2's (kiln-bp's
//! `Redeem`) when it lands, with the same rules.
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
use parking_lot::Mutex;
use serde_json::{Value, json};
use sha2::Digest as _;

use crate::hub::Hub;

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
    Unknown,
    Used,
    Expired,
}

/// What mints and redeems the door's tickets: this module's [`Tickets`] until goal 2's lands.
pub trait DoorTickets: Send {
    /// A ticket of the caller's random bytes for `principal`.
    fn mint(&mut self, random: [u8; 32], principal: &str, now_ms: u64) -> Vec<u8>;
    /// Redeems a ticket, once: whom it was minted for.
    fn redeem(&mut self, ticket: &[u8], now_ms: u64) -> Result<String, Refusal>;
    /// Tickets held.
    fn held(&self) -> usize;
}

/// Single-use tickets that expire, as goal 2's: forgotten past twice their life, so one shown
/// after that reads as never made.
#[derive(Default)]
pub struct Tickets {
    minted: BTreeMap<Vec<u8>, (String, u64, bool)>,
}

impl Tickets {
    fn forget(&mut self, now: u64) {
        self.minted.retain(|_, (_, at, _)| now.saturating_sub(*at) <= 2 * TICKET_LIFE_MS);
    }
}

impl DoorTickets for Tickets {
    fn mint(&mut self, random: [u8; 32], principal: &str, now_ms: u64) -> Vec<u8> {
        self.forget(now_ms);
        self.minted.insert(random.to_vec(), (principal.to_owned(), now_ms, false));
        random.to_vec()
    }
    fn redeem(&mut self, ticket: &[u8], now_ms: u64) -> Result<String, Refusal> {
        self.forget(now_ms);
        let (who, at, taken) = self.minted.get_mut(ticket).ok_or(Refusal::Unknown)?;
        if *taken {
            return Err(Refusal::Used);
        }
        if now_ms.saturating_sub(*at) > TICKET_LIFE_MS {
            return Err(Refusal::Expired);
        }
        *taken = true;
        Ok(who.clone())
    }
    fn held(&self) -> usize {
        self.minted.len()
    }
}

// ------------------------------------------------------------------------------- admission

/// The paths the door serves: `/wt` (the browser protocol's, goal 5's to serve) and `/wt/echo`
/// (echoes, for the lab's browser checks).
pub const PATHS: [&str; 2] = ["/wt", "/wt/echo"];

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// Decides a CONNECT: an origin of the server's own, a path it serves, a ticket not yet used and
/// minted for the principal the path names (`?ticket=…&u=…`).
pub fn admit(c: &Connect, origins: &[String], tickets: &mut dyn DoorTickets, now_ms: u64) -> Admission {
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
            Some(("u", u)) => user = Some(u),
            _ => {}
        }
    }
    let Some(ticket) = ticket else { return Admission::Forbidden };
    match tickets.redeem(&ticket, now_ms) {
        Ok(who) if user.is_some_and(|u| u == url_part(&who)) => Admission::Accept,
        _ => Admission::Forbidden,
    }
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

/// What the HTTP API and the door share: the hashes served, the tickets, the URL.
pub struct Shared {
    pub url: String,
    /// The pinned hashes, or none for a publicly trusted certificate.
    pub hashes: Vec<[u8; 32]>,
    pub tickets: Box<dyn DoorTickets>,
    pub origins: Vec<String>,
}

static DOOR: OnceLock<Arc<Mutex<Shared>>> = OnceLock::new();

fn now_ms() -> u64 {
    u64::try_from(time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(0)
}

/// `POST /api/me/door`: the door's URL, the hashes to pin and a ticket for the caller, or that the
/// door is off.
pub fn info(principal: &str) -> Value {
    match DOOR.get() {
        None => json!({ "on": false }),
        Some(s) => info_of(&mut s.lock(), principal, rand::random(), now_ms()),
    }
}

/// [`info`] over given state (the tests' way in).
pub fn info_of(s: &mut Shared, principal: &str, random: [u8; 32], now_ms: u64) -> Value {
    let ticket = s.tickets.mint(random, principal, now_ms);
    json!({
        "on": true,
        "url": format!("{}?ticket={}&u={}", s.url, b64(&ticket), url_part(principal)),
        "hashes": s.hashes.iter().map(|h| base64::engine::general_purpose::STANDARD.encode(h)).collect::<Vec<_>>(),
        "ticket_life_ms": TICKET_LIFE_MS,
    })
}

/// The door's host: tickets decide CONNECTs; `/wt/echo` sessions echo; rotation happens on turns.
struct DoorHost {
    shared: Arc<Mutex<Shared>>,
    certs: Option<(Certs, Vec<String>)>,
    shutdown: crate::rt::Shutdown,
}

impl Admit for DoorHost {
    fn admit(&mut self, c: &Connect) -> Admission {
        let mut s = self.shared.lock();
        let origins = s.origins.clone();
        admit(c, &origins, s.tickets.as_mut(), now_ms())
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
                    self.shared.lock().hashes = certs.hashes().to_vec();
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
    let shared = Arc::new(Mutex::new(Shared { url: format!("https://{host}/wt"), hashes, tickets: Box::new(Tickets::default()), origins: origins(&s.public_url, &s.tls_names) }));
    let _ = DOOR.set(Arc::clone(&shared));
    tracing::info!(%bind, %host, "the HTTP/3 WebTransport door");
    let mut h = DoorHost { shared, certs, shutdown: hub.shutdown.clone() };
    driver.run(&mut h).await.map_err(|e| anyhow::anyhow!("the HTTP/3 door: {e}"))
}

#[cfg(test)]
mod tests;
