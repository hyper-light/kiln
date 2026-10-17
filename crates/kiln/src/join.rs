//! Joining a kiln server from another computer, with an admin's approval (`kiln join`).
//!
//! ```text
//!  new machine (kiln join)                        server                              admin (UI)
//!  key + CSR; secret s; commitment H(s)
//!  POST /api/join-requests {csr, H(s), …}   ──▶  kept in memory for 10 minutes
//!                                          ◀──  {id, nonce, ca}
//!  code = SAS(public key, CA, s, nonce)
//!  GET /api/join-requests/<id> (Bearer s)   ──▶  checks H(s); can now show the code ──▶  Waiting to join: code
//!     … every 2 s …                                                                  ◀──  Approve (codes match)
//!                                          ◀──  {approved, cert, CA, QUIC}       signs the CSR like enrollment
//!  stores the credentials, installs the agent
//! ```
//!
//! **The verification code** is six digits of SHA-256 over the CSR's public key, the server's CA
//! certificate, the machine's secret and the server's nonce. The machine commits to its secret
//! (sends only its hash) before it learns the nonce, and the server picks the nonce after the CSR is
//! fixed, so neither a second machine nor a man in the middle can make the code on the machine and
//! the code in the UI agree except by a one-in-a-million guess per attempt, and attempts are rate
//! limited. Nothing is ever approved automatically.
//!
//! The server keeps requests in memory only: at most [`MAX_PENDING`] waiting, one per source address
//! every [`PER_SOURCE`], each for [`REQUEST_TTL`]. A machine whose request vanished (server restart,
//! expiry) asks again, with a new code.

use std::collections::HashMap;
use std::io::IsTerminal as _;
use std::net::IpAddr;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::discovery::Instance;
use crate::http::client::Client;
use crate::rt::Shutdown;

/// How long a join request lives, whatever its state.
pub const REQUEST_TTL: Duration = Duration::from_secs(600);
/// Most requests waiting for a decision at once.
pub const MAX_PENDING: usize = 20;
/// One request per source address this often.
pub const PER_SOURCE: Duration = Duration::from_secs(5);
/// Largest CSR accepted (PEM bytes).
pub const CSR_LIMIT: usize = 16 << 10;
/// Most requests kept, decided ones included.
const MAX_ENTRIES: usize = 128;
const POLL_EVERY: Duration = Duration::from_secs(2);
/// How often a machine asks again after its request vanished.
const RESUBMITS: u32 = 5;

// ------------------------------------------------------------------------------ the code

/// The six-digit verification code both sides show (see the module docs).
pub fn verification_code(public_key: &[u8], ca_der: &[u8], secret: &[u8], nonce: &[u8]) -> String {
    use sha2::Digest as _;
    let mut h = sha2::Sha256::new();
    h.update(b"kiln join verification v1");
    for part in [public_key, ca_der, secret, nonce] {
        h.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
        h.update(part);
    }
    let n = h.finalize().iter().take(8).fold(0u64, |acc, b| (acc << 8) | u64::from(*b));
    format!("{:06}", n % 1_000_000)
}

/// `123456` → `123 456`.
pub fn display_code(code: &str) -> String {
    match (code.get(..3), code.get(3..)) {
        (Some(a), Some(b)) if code.len() == 6 => format!("{a} {b}"),
        _ => code.to_owned(),
    }
}

/// The public key (SubjectPublicKeyInfo, DER) of a PEM CSR, after checking the CSR's signature.
pub fn csr_public_key(csr_pem: &str) -> anyhow::Result<Vec<u8>> {
    use rcgen::PublicKeyData as _;
    let csr = rcgen::CertificateSigningRequestParams::from_pem(csr_pem)
        .map_err(|e| anyhow::anyhow!("not a valid certificate signing request: {e}"))?;
    Ok(csr.public_key.subject_public_key_info())
}

/// The raw public key bits of a CSR and of a certificate (to check a certificate is for our key).
fn csr_key_bits(csr_pem: &str) -> Option<Vec<u8>> {
    use rcgen::PublicKeyData as _;
    rcgen::CertificateSigningRequestParams::from_pem(csr_pem).ok().map(|c| c.public_key.der_bytes().to_vec())
}

fn cert_key_bits(cert_pem: &str) -> Option<Vec<u8>> {
    let der = pem_der(cert_pem).ok()?;
    let (_, cert) = x509_parser::parse_x509_certificate(&der).ok()?;
    Some(cert.public_key().subject_public_key.data.to_vec())
}

pub fn pem_der(pem: &str) -> anyhow::Result<Vec<u8>> {
    use hyper_quic::rustls::pki_types::CertificateDer;
    use hyper_quic::rustls::pki_types::pem::PemObject as _;
    Ok(CertificateDer::from_pem_slice(pem.as_bytes()).map_err(|e| anyhow::anyhow!("certificate pem: {e:?}"))?.to_vec())
}

// ------------------------------------------------------------------------------ wire types

/// `POST /api/join-requests` (no authentication).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct NewRequest {
    pub name: String,
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub version: String,
    /// PEM certificate signing request; the private key stays on the machine.
    pub csr: String,
    /// SHA-256 (hex) of the secret the machine polls with.
    pub commitment: String,
}

/// The answer to a new request.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Created {
    pub id: String,
    /// The server's nonce (hex), part of the verification code.
    pub nonce: String,
    /// The server's CA certificate, part of the verification code.
    pub ca_pem: String,
    pub expires_in: u64,
    /// The server's host name and version, for the machine's messages.
    pub server: String,
    pub version: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    Approved,
    Denied,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Denied => "denied",
        }
    }
}

/// What an approved machine receives: its certificate and where to connect.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Approval {
    /// The agent name the certificate carries (the requested one, or a free variant of it).
    pub name: String,
    pub labels: Vec<String>,
    pub cert_pem: String,
    pub ca_pem: String,
    /// The QUIC address the server advertises, and its port.
    pub quic: String,
    pub quic_port: u16,
}

/// `GET /api/join-requests/<id>` with the machine's secret.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Polled {
    pub id: String,
    pub status: Status,
    pub code: Option<String>,
    pub expires_in: u64,
    #[serde(default)]
    pub approval: Option<Approval>,
}

/// A request as admins see it (`GET /api/join-requests`).
#[derive(Serialize, Clone, Debug)]
pub struct Listed {
    pub id: String,
    pub name: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub labels: Vec<String>,
    pub version: String,
    /// Where the request came from.
    pub address: String,
    pub status: Status,
    /// `null` until the machine has confirmed the request (its first poll); approval needs it.
    pub code: Option<String>,
    pub created_at: i64,
    pub expires_at: i64,
    /// The agent name the approval gave.
    pub agent: Option<String>,
}

/// `POST /api/join-requests/<id>/approve` (optional body).
#[derive(Deserialize, Default, Debug)]
pub struct Decision {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub labels: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    Invalid(String),
    TooMany { retry_after: Duration, why: &'static str },
    Unknown,
    WrongSecret,
    NotPending(Status),
    Unconfirmed,
}

impl Refusal {
    pub fn status(&self) -> u16 {
        match self {
            Self::Invalid(_) => 400,
            Self::TooMany { .. } => 429,
            Self::Unknown => 404,
            Self::WrongSecret => 403,
            Self::NotPending(_) | Self::Unconfirmed => 409,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Invalid(why) => why.clone(),
            Self::TooMany { why, .. } => (*why).to_owned(),
            Self::Unknown => "no such join request (it may have expired)".to_owned(),
            Self::WrongSecret => "wrong join secret".to_owned(),
            Self::NotPending(s) => format!("the request was already {}", s.as_str()),
            Self::Unconfirmed => "the machine has not confirmed the request yet; its verification code is not known".to_owned(),
        }
    }
}

// ------------------------------------------------------------------------------ the server's side

/// What an approval needs from a pending request.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub name: String,
    pub hostname: String,
    pub labels: Vec<String>,
    pub csr: String,
}

struct Entry {
    id: String,
    name: String,
    hostname: String,
    os: String,
    arch: String,
    labels: Vec<String>,
    version: String,
    csr: String,
    public_key: Vec<u8>,
    commitment: String,
    nonce: [u8; 16],
    /// Revealed by the machine's first poll.
    secret: Option<String>,
    address: IpAddr,
    created: Instant,
    created_unix: i64,
    status: Status,
    approval: Option<Approval>,
}

impl Entry {
    fn code(&self, ca_der: &[u8]) -> Option<String> {
        self.secret.as_ref().map(|s| verification_code(&self.public_key, ca_der, s.as_bytes(), &self.nonce))
    }

    fn expires_in(&self, now: Instant) -> u64 {
        REQUEST_TTL.saturating_sub(now.saturating_duration_since(self.created)).as_secs()
    }

    fn listed(&self, ca_der: &[u8]) -> Listed {
        Listed {
            id: self.id.clone(),
            name: self.name.clone(),
            hostname: self.hostname.clone(),
            os: self.os.clone(),
            arch: self.arch.clone(),
            // What the agent got, once approved (an admin may have changed the labels).
            labels: self.approval.as_ref().map_or_else(|| self.labels.clone(), |a| a.labels.clone()),
            version: self.version.clone(),
            address: self.address.to_string(),
            status: self.status,
            code: self.code(ca_der),
            created_at: self.created_unix,
            expires_at: self.created_unix + i64::try_from(REQUEST_TTL.as_secs()).unwrap_or(600),
            agent: self.approval.as_ref().map(|a| a.name.clone()),
        }
    }
}

#[derive(Default)]
struct Inner {
    entries: HashMap<String, Entry>,
    last_from: HashMap<IpAddr, Instant>,
}

impl Inner {
    fn purge(&mut self, now: Instant) {
        self.entries.retain(|_, e| now.saturating_duration_since(e.created) < REQUEST_TTL);
        self.last_from.retain(|_, t| now.saturating_duration_since(*t) < PER_SOURCE);
    }
}

/// The server's join requests (in memory).
#[derive(Default)]
pub struct Requests {
    inner: Mutex<Inner>,
}

/// A host name or version string from the network: printable, at most `max` characters.
fn text(s: &str, max: usize) -> String {
    s.chars().filter(|c| !c.is_control()).take(max).collect::<String>().trim().to_owned()
}

/// An OS or architecture name: lowercase `[a-z0-9_-]`.
fn word(s: &str, max: usize) -> String {
    let w: String = s
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .take(max)
        .collect::<String>()
        .to_ascii_lowercase();
    if w.is_empty() { "unknown".to_owned() } else { w }
}

/// Labels as `runs-on` can use them: lowercase `[a-z0-9._-]`, deduplicated, at most 16.
pub fn clean_labels(labels: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for l in labels {
        let c: String = l
            .trim()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' { c.to_ascii_lowercase() } else { '-' })
            .take(63)
            .collect();
        let c = c.trim_matches('-').to_owned();
        if !c.is_empty() && !out.contains(&c) && out.len() < 16 {
            out.push(c);
        }
    }
    out
}

/// `[baremetal, <os>, <arch>]`.
pub fn default_labels(os: &str, arch: &str) -> Vec<String> {
    clean_labels(&["baremetal".to_owned(), os.to_owned(), arch.to_owned()])
}

impl Requests {
    /// A new request from `from`. Returns its id and the server's nonce.
    pub fn create(&self, req: NewRequest, from: IpAddr, now: Instant) -> Result<(String, [u8; 16]), Refusal> {
        let mut g = self.inner.lock();
        g.purge(now);
        if let Some(t) = g.last_from.get(&from) {
            let since = now.saturating_duration_since(*t);
            if since < PER_SOURCE {
                return Err(Refusal::TooMany { retry_after: PER_SOURCE - since, why: "one join request per address every 5 seconds" });
            }
        }
        let pending: Vec<Instant> = g.entries.values().filter(|e| e.status == Status::Pending).map(|e| e.created).collect();
        if pending.len() >= MAX_PENDING {
            let oldest = pending.iter().min().copied().unwrap_or(now);
            return Err(Refusal::TooMany {
                retry_after: REQUEST_TTL.saturating_sub(now.saturating_duration_since(oldest)).max(Duration::from_secs(1)),
                why: "too many join requests are waiting; approve or deny some first",
            });
        }
        g.last_from.insert(from, now);
        if req.csr.len() > CSR_LIMIT {
            return Err(Refusal::Invalid("the CSR is too large".to_owned()));
        }
        let public_key = csr_public_key(&req.csr).map_err(|e| Refusal::Invalid(format!("{e:#}")))?;
        let commitment = req.commitment.trim().to_ascii_lowercase();
        if commitment.len() != 64 || !commitment.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Refusal::Invalid("commitment must be the SHA-256 (hex) of the join secret".to_owned()));
        }
        let hostname = text(&req.hostname, 64);
        let wanted = if req.name.trim().is_empty() { hostname.as_str() } else { req.name.as_str() };
        let name = crate::pki::sanitize_name(wanted);
        let os = word(&req.os, 32);
        let arch = word(&req.arch, 32);
        let mut labels = clean_labels(&req.labels);
        if labels.is_empty() {
            labels = default_labels(&os, &arch);
        }
        // Room for it: decided requests make way, oldest first.
        while g.entries.len() >= MAX_ENTRIES {
            let oldest = g.entries.values().filter(|e| e.status != Status::Pending).min_by_key(|e| e.created).map(|e| e.id.clone());
            match oldest {
                Some(id) => {
                    g.entries.remove(&id);
                }
                None => return Err(Refusal::TooMany { retry_after: Duration::from_secs(60), why: "too many join requests" }),
            }
        }
        let id = crate::util::new_id();
        let nonce: [u8; 16] = rand::random();
        g.entries.insert(
            id.clone(),
            Entry {
                id: id.clone(),
                name,
                hostname,
                os,
                arch,
                labels,
                version: text(&req.version, 32),
                csr: req.csr,
                public_key,
                commitment,
                nonce,
                secret: None,
                address: from,
                created: now,
                created_unix: crate::util::now_unix(),
                status: Status::Pending,
                approval: None,
            },
        );
        Ok((id, nonce))
    }

    /// The machine's view of its request. Its first poll confirms the request (reveals the secret, so
    /// the code can be shown to admins); the flag says whether this call did.
    pub fn poll(&self, id: &str, secret: &str, now: Instant, ca_der: &[u8]) -> Result<(Polled, bool), Refusal> {
        let mut g = self.inner.lock();
        g.purge(now);
        let e = g.entries.get_mut(id).ok_or(Refusal::Unknown)?;
        let hash = crate::util::sha256_hex(secret.as_bytes());
        if !crate::util::ct_eq(hash.as_bytes(), e.commitment.as_bytes()) {
            return Err(Refusal::WrongSecret);
        }
        let confirmed = e.secret.is_none();
        if confirmed {
            e.secret = Some(secret.to_owned());
        }
        let polled = Polled {
            id: e.id.clone(),
            status: e.status,
            code: e.code(ca_der),
            expires_in: e.expires_in(now),
            approval: e.approval.clone(),
        };
        Ok((polled, confirmed))
    }

    /// Every live request, oldest first.
    pub fn list(&self, now: Instant, ca_der: &[u8]) -> Vec<Listed> {
        let mut g = self.inner.lock();
        g.purge(now);
        let mut v: Vec<(Instant, Listed)> = g.entries.values().map(|e| (e.created, e.listed(ca_der))).collect();
        v.sort_by_key(|(t, _)| *t);
        v.into_iter().map(|(_, l)| l).collect()
    }

    pub fn get(&self, id: &str, now: Instant, ca_der: &[u8]) -> Result<Listed, Refusal> {
        let mut g = self.inner.lock();
        g.purge(now);
        g.entries.get(id).map(|e| e.listed(ca_der)).ok_or(Refusal::Unknown)
    }

    pub fn pending(&self, now: Instant) -> usize {
        let mut g = self.inner.lock();
        g.purge(now);
        g.entries.values().filter(|e| e.status == Status::Pending).count()
    }

    /// A request that can be approved: pending and confirmed by its machine.
    pub fn candidate(&self, id: &str, now: Instant) -> Result<Candidate, Refusal> {
        let mut g = self.inner.lock();
        g.purge(now);
        let e = g.entries.get(id).ok_or(Refusal::Unknown)?;
        if e.status != Status::Pending {
            return Err(Refusal::NotPending(e.status));
        }
        if e.secret.is_none() {
            return Err(Refusal::Unconfirmed);
        }
        Ok(Candidate { name: e.name.clone(), hostname: e.hostname.clone(), labels: e.labels.clone(), csr: e.csr.clone() })
    }

    /// Record the approval (the certificate was signed by the caller).
    pub fn approve(&self, id: &str, approval: Approval, now: Instant) -> Result<(), Refusal> {
        let mut g = self.inner.lock();
        g.purge(now);
        let e = g.entries.get_mut(id).ok_or(Refusal::Unknown)?;
        if e.status != Status::Pending {
            return Err(Refusal::NotPending(e.status));
        }
        e.status = Status::Approved;
        e.approval = Some(approval);
        Ok(())
    }

    /// Deny a pending request; the machine learns it on its next poll. Returns its name.
    pub fn deny(&self, id: &str, now: Instant) -> Result<String, Refusal> {
        let mut g = self.inner.lock();
        g.purge(now);
        let e = g.entries.get_mut(id).ok_or(Refusal::Unknown)?;
        if e.status != Status::Pending {
            return Err(Refusal::NotPending(e.status));
        }
        e.status = Status::Denied;
        Ok(e.name.clone())
    }
}

// ------------------------------------------------------------------------------ the machine's side

/// The machine asking to join.
#[derive(Clone, Debug)]
pub struct Machine {
    pub name: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub labels: Vec<String>,
}

impl Machine {
    /// This machine: its host name, OS and architecture; labels default to `[baremetal, os, arch]`.
    pub fn this(name: Option<String>, labels: &[String]) -> Self {
        let hostname = crate::config::Config::hostname();
        let os = std::env::consts::OS.to_owned();
        let arch = std::env::consts::ARCH.to_owned();
        let labels = match clean_labels(labels) {
            l if l.is_empty() => default_labels(&os, &arch),
            l => l,
        };
        Self { name: crate::pki::sanitize_name(&name.unwrap_or_else(|| hostname.clone())), hostname, os, arch, labels }
    }
}

/// A completed join: credentials are stored in the state directory.
#[derive(Clone, Debug)]
pub struct Joined {
    pub server: String,
    pub name: String,
    pub labels: Vec<String>,
    pub quic: String,
    pub ca_der: Vec<u8>,
}

/// Whether `state_dir` holds agent credentials.
pub fn enrolled(state_dir: &Path) -> bool {
    ["agent.pem", "agent.key", "ca.pem"].iter().all(|f| state_dir.join(f).is_file())
}

/// Read a small text file from the state directory (the server URL or labels a join stored).
pub fn stored(state_dir: &Path, file: &str) -> Option<String> {
    std::fs::read_to_string(state_dir.join(file)).ok().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

fn error_text(body: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_owned))
        .unwrap_or_else(|| crate::util::truncate(&String::from_utf8_lossy(body), 300).to_owned())
}

async fn submit(http: &Client, server: &str, me: &Machine, csr: &str, secret: &str, say: &(dyn Fn(&str) + Send + Sync), shutdown: &Shutdown) -> anyhow::Result<Created> {
    let body = NewRequest {
        name: me.name.clone(),
        hostname: me.hostname.clone(),
        os: me.os.clone(),
        arch: me.arch.clone(),
        labels: me.labels.clone(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        csr: csr.to_owned(),
        commitment: crate::util::sha256_hex(secret.as_bytes()),
    };
    let url = format!("{server}/api/join-requests");
    let mut failures = 0u32;
    loop {
        let wait = match http.post(&url).json(&body).timeout(Duration::from_secs(10)).send().await {
            Ok(resp) if resp.is_success() => return resp.json(1 << 20).await.context("the server's answer to the join request"),
            Ok(resp) if resp.status() == 429 => {
                let secs = resp.header("retry-after").and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(5).clamp(1, 600);
                let why = error_text(&resp.bytes(64 << 10).await.unwrap_or_default());
                say(&format!("The server is busy ({why}); asking again in {secs} s…"));
                Duration::from_secs(secs)
            }
            Ok(resp) if resp.status() == 404 => {
                anyhow::bail!("{server} does not accept join requests (an older kiln, or `[discovery] join_requests = false`)")
            }
            Ok(resp) if (400..500).contains(&resp.status()) => {
                let status = resp.status();
                let why = error_text(&resp.bytes(64 << 10).await.unwrap_or_default());
                anyhow::bail!("{server} refused the join request (HTTP {status}): {why}")
            }
            Ok(resp) => {
                failures += 1;
                say(&format!("The server answered HTTP {}; trying again…", resp.status()));
                Duration::from_secs(2)
            }
            Err(e) => {
                failures += 1;
                if failures >= 3 {
                    return Err(e).with_context(|| {
                        format!("cannot reach {server}. Is the URL right, and are its HTTP and QUIC ports open to this machine (firewall)?")
                    });
                }
                Duration::from_secs(2)
            }
        };
        if failures > 30 {
            anyhow::bail!("{server} keeps failing; giving up");
        }
        if !shutdown.sleep(wait).await {
            anyhow::bail!("interrupted");
        }
    }
}

enum PollError {
    /// The server no longer has the request (restart, expiry).
    Gone,
    Fatal(anyhow::Error),
    Retry(anyhow::Error),
}

async fn poll(http: &Client, server: &str, id: &str, secret: &str) -> Result<Polled, PollError> {
    let resp = http
        .get(&format!("{server}/api/join-requests/{id}"))
        .bearer(secret)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(PollError::Retry)?;
    match resp.status() {
        200..=299 => resp.json(1 << 20).await.map_err(PollError::Retry),
        404 => Err(PollError::Gone),
        s @ (401 | 403) => Err(PollError::Fatal(anyhow::anyhow!(
            "the server refused this machine's secret (HTTP {s}): {}",
            error_text(&resp.bytes(64 << 10).await.unwrap_or_default())
        ))),
        s => Err(PollError::Retry(anyhow::anyhow!("HTTP {s}"))),
    }
}

fn host_of(url: &str) -> Option<String> {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let authority = rest.split('/').next()?;
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split(']').next()?
    } else {
        authority.rsplit_once(':').map_or(authority, |(h, _)| h)
    };
    (!host.is_empty()).then(|| host.to_owned())
}

fn host_port(host: &str, port: u16) -> String {
    if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") }
}

/// Where the agent dials QUIC: the advertised address when it is usable from here, else the host
/// this machine reached the server's HTTP at (an IP from discovery always is).
async fn quic_address(server: &str, a: &Approval) -> String {
    let Some(url_host) = host_of(server) else { return a.quic.clone() };
    let fallback = host_port(&url_host, a.quic_port);
    if url_host.parse::<IpAddr>().is_ok() {
        return fallback;
    }
    match host_of(&format!("x://{}", a.quic)) {
        Some(h) if h.eq_ignore_ascii_case(&url_host) => a.quic.clone(),
        Some(_) if crate::rt::resolve(&a.quic).await.is_ok() => a.quic.clone(),
        _ => fallback,
    }
}

fn store(state_dir: &Path, server: &str, key_pem: &str, a: &Approval, quic: &str) -> anyhow::Result<()> {
    use crate::util::write_secret;
    std::fs::create_dir_all(state_dir).with_context(|| format!("create {}", state_dir.display()))?;
    write_secret(&state_dir.join("agent.key"), key_pem.as_bytes(), 0o600)?;
    write_secret(&state_dir.join("agent.pem"), a.cert_pem.as_bytes(), 0o644)?;
    write_secret(&state_dir.join("ca.pem"), a.ca_pem.as_bytes(), 0o644)?;
    write_secret(&state_dir.join("quic"), quic.as_bytes(), 0o644)?;
    write_secret(&state_dir.join("server"), server.as_bytes(), 0o644)?;
    write_secret(&state_dir.join("labels"), a.labels.join(",").as_bytes(), 0o644)?;
    Ok(())
}

/// Ask `server` to let this machine join, show the verification code (through `say`), wait for the
/// admin's decision and store the credentials in `state_dir`, like enrollment does.
pub async fn request(server: &str, me: &Machine, state_dir: &Path, say: &(dyn Fn(&str) + Send + Sync), shutdown: &Shutdown) -> anyhow::Result<Joined> {
    let server = server.trim().trim_end_matches('/').to_owned();
    anyhow::ensure!(server.starts_with("http://") || server.starts_with("https://"), "the server URL must start with http:// or https://");
    let (csr, key_pem) = crate::pki::make_csr(&me.name)?;
    let public_key = csr_public_key(&csr)?;
    let key_bits = csr_key_bits(&csr).context("read the CSR's key")?;
    let http = Client::public()?;
    let mut attempts = 0u32;
    'submit: loop {
        attempts += 1;
        if attempts > RESUBMITS + 1 {
            anyhow::bail!("the join request kept expiring without a decision; run `kiln join` again when the admin is ready");
        }
        let secret = crate::util::random_token();
        let created = submit(&http, &server, me, &csr, &secret, say, shutdown).await?;
        let ca_der = pem_der(&created.ca_pem).context("the server's CA certificate")?;
        let nonce = hex::decode(created.nonce.trim()).context("the server's nonce")?;
        let code = verification_code(&public_key, &ca_der, secret.as_bytes(), &nonce);
        say(&format!(
            "Asked {} ({server}, kiln {}) to let this machine join as \"{}\" ({}/{}, labels {}).\n\n    Verification code:  {}\n\n\
             Approve it in the kiln UI under Agents → Waiting to join, after checking that the UI shows the same code.\n\
             Waiting for a decision (the request expires in {} minutes)…",
            created.server,
            created.version,
            me.name,
            me.os,
            me.arch,
            me.labels.join(", "),
            display_code(&code),
            created.expires_in / 60,
        ));
        let mut transient = 0u32;
        loop {
            match poll(&http, &server, &created.id, &secret).await {
                Ok(p) => {
                    transient = 0;
                    if let Some(theirs) = &p.code
                        && *theirs != code
                    {
                        anyhow::bail!(
                            "the server shows a different verification code ({}) than this machine ({}); something between them changed \
                             the request. Not joining.",
                            display_code(theirs),
                            display_code(&code)
                        );
                    }
                    match p.status {
                        Status::Pending => {}
                        Status::Denied => anyhow::bail!("the request was denied by the server's admin"),
                        Status::Approved => {
                            let a = p.approval.context("the server approved without sending credentials")?;
                            anyhow::ensure!(
                                pem_der(&a.ca_pem).ok().as_deref() == Some(ca_der.as_slice()),
                                "the approval came with a different CA than the one the verification code covers; not joining"
                            );
                            anyhow::ensure!(
                                cert_key_bits(&a.cert_pem).as_deref() == Some(key_bits.as_slice()),
                                "the approved certificate is not for this machine's key; not joining"
                            );
                            let quic = quic_address(&server, &a).await;
                            store(state_dir, &server, &key_pem, &a, &quic)?;
                            say(&format!("Approved: this machine is agent \"{}\" (labels {}).", a.name, a.labels.join(", ")));
                            return Ok(Joined { server, name: a.name, labels: a.labels, quic, ca_der });
                        }
                    }
                }
                Err(PollError::Gone) => {
                    say("The server no longer has the request (it restarted, or the request expired). Asking again…");
                    continue 'submit;
                }
                Err(PollError::Fatal(e)) => return Err(e),
                Err(PollError::Retry(e)) => {
                    transient += 1;
                    if transient == 3 {
                        say(&format!("Cannot reach the server right now ({e:#}); still waiting…"));
                    }
                }
            }
            if !shutdown.sleep(POLL_EVERY).await {
                anyhow::bail!("interrupted; the request stays pending on the server until it expires");
            }
        }
    }
}

// ------------------------------------------------------------------------------ picking a server

/// The instance id of a kiln server on this machine (to leave it out when looking for servers).
fn local_server_id() -> Option<String> {
    let cfg = crate::config::Config::load(&crate::config::default_config_path()).unwrap_or_default();
    std::fs::read_to_string(cfg.server.data_dir.join("instance.id")).ok().map(|s| s.trim().to_ascii_lowercase())
}

/// The kiln servers among `found` that this machine could join.
pub fn joinable(found: Vec<Instance>) -> Vec<Instance> {
    let mine = local_server_id();
    found
        .into_iter()
        .filter(|i| i.role == "server" && i.url().is_some() && mine.as_deref() != Some(i.id.as_str()))
        .collect()
}

pub fn say_stderr(msg: &str) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr(), "{msg}");
}

/// Read a line from the terminal (on the blocking pool: it waits for a human). Ctrl-C (`shutdown`)
/// abandons the question.
async fn ask(prompt: &str, shutdown: &Shutdown) -> anyhow::Result<String> {
    use futures::FutureExt as _;
    use std::io::Write as _;
    let mut err = std::io::stderr();
    let _ = write!(err, "{prompt}");
    let _ = err.flush();
    let read = crate::rt::blocking(|| {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).map(|_| line.trim().to_owned())
    })
    .fuse();
    let stop = shutdown.cancelled().fuse();
    futures::pin_mut!(read, stop);
    futures::select_biased! {
        () = stop => anyhow::bail!("interrupted"),
        line = read => line?.context("read the answer"),
    }
}

fn yes(answer: &str) -> bool {
    answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes")
}

fn describe(i: &Instance) -> String {
    format!("{} — {} (kiln {}, {}/{})", i.name, i.url().unwrap_or_default(), i.version, i.os, i.arch)
}

/// Pick the server to join among those found: the only one (with `yes`, or after asking), or the
/// one the user chooses.
pub async fn choose(servers: Vec<Instance>, yes_to_one: bool, interactive: bool, shutdown: &Shutdown) -> anyhow::Result<String> {
    let yes = yes_to_one;
    match servers.as_slice() {
        [] => anyhow::bail!(
            "no kiln server found on this network. Pass --server http://<server>:7070, or check that UDP 5353 (multicast DNS) \
             is open on both machines (e.g. `sudo ufw allow from 192.168.1.0/24 to any port 5353 proto udp`)"
        ),
        [only] => {
            let url = only.url().unwrap_or_default();
            if yes {
                say_stderr(&format!("Found {}.", describe(only)));
                return Ok(url);
            }
            anyhow::ensure!(interactive, "found {}; pass --yes to join it (or --server)", describe(only));
            let answer = ask(&format!("Found {}.\nJoin it? [Y/n] ", describe(only)), shutdown).await?;
            anyhow::ensure!(answer.is_empty() || self::yes(&answer), "not joining");
            Ok(url)
        }
        many => {
            anyhow::ensure!(
                interactive,
                "found {} kiln servers; pass --server to choose one:\n  {}",
                many.len(),
                many.iter().map(describe).collect::<Vec<_>>().join("\n  ")
            );
            say_stderr("Kiln servers on this network:");
            for (n, i) in many.iter().enumerate() {
                say_stderr(&format!("  {}) {}", n + 1, describe(i)));
            }
            let answer = ask(&format!("Which one to join? [1-{}] ", many.len()), shutdown).await?;
            let n: usize = answer.parse().context("expected a number")?;
            many.get(n.wrapping_sub(1)).and_then(Instance::url).context("no such server")
        }
    }
}

/// Look for joinable servers on the network for `wait`.
pub async fn find_servers(wait: Duration, interfaces: &[String]) -> anyhow::Result<Vec<Instance>> {
    Ok(joinable(crate::discovery::browse_once(wait, interfaces).await?))
}

/// The joinable servers a running discovery table heard: waits until one shows up (and a moment
/// longer, to hear the others), at most `max`.
pub async fn servers_in(table: &crate::discovery::Table, max: Duration) -> Vec<Instance> {
    let start = Instant::now();
    loop {
        let found = joinable(table.snapshot());
        if (!found.is_empty() && start.elapsed() >= Duration::from_millis(1500)) || start.elapsed() >= max {
            return found;
        }
        crate::rt::sleep(Duration::from_millis(250)).await;
    }
}

/// `kiln up` on a machine that never ran a server: offer to join a kiln server on the network as an
/// agent instead of starting another server. True when this machine joined.
pub async fn offer_instead_of_server(config_path: &Path, cfg: &crate::config::Config) -> anyhow::Result<bool> {
    let fresh = !config_path.exists() && !cfg.server.data_dir.join("kiln.db").exists();
    if !fresh || !cfg.discovery.enabled || !interactive() {
        return Ok(false);
    }
    let shutdown = Shutdown::new();
    let servers = find_servers(Duration::from_secs(2), &cfg.discovery.interfaces).await.unwrap_or_default();
    if servers.is_empty() {
        return Ok(false);
    }
    say_stderr(&format!(
        "No kiln server is set up on this machine, and there is already one on your network:\n  {}",
        servers.iter().map(describe).collect::<Vec<_>>().join("\n  ")
    ));
    let answer = ask("Join it as an agent instead of starting a new server here? [y/N] ", &shutdown).await?;
    if !yes(&answer) {
        return Ok(false);
    }
    let server = choose(servers, true, true, &shutdown).await?;
    let args = JoinArgs {
        server: Some(server),
        yes: false,
        name: None,
        labels: Vec::new(),
        slots: None,
        no_install: false,
        boot: crate::install::Boot::Auto,
        wait: 3,
    };
    cli(args, shutdown).await?;
    Ok(true)
}

pub fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

// ------------------------------------------------------------------------------ `kiln join`

#[derive(clap::Args, Clone, Debug)]
pub struct JoinArgs {
    /// The kiln server to join, e.g. http://192.168.1.10:7070 (default: look for servers on this network).
    #[arg(long, env = "KILN_SERVER")]
    pub server: Option<String>,
    /// Join the only server found without asking, and replace an existing enrollment.
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// This machine's agent name (default: its host name).
    #[arg(long)]
    pub name: Option<String>,
    /// Labels jobs target with `runs-on` (default: baremetal,<os>,<arch>).
    #[arg(long, value_delimiter = ',')]
    pub labels: Vec<String>,
    /// Job slots (default: a quarter of the CPUs).
    #[arg(long)]
    pub slots: Option<u32>,
    /// Store the credentials and run the agent in the foreground instead of installing it to start on boot.
    #[arg(long)]
    pub no_install: bool,
    /// How the installed agent starts on boot.
    #[arg(long, value_enum, default_value = "auto")]
    pub boot: crate::install::Boot,
    /// Seconds to look for servers on the network.
    #[arg(long, default_value_t = 3)]
    pub wait: u64,
}

/// `kiln join`. Returns the agent to run in the foreground with `--no-install`.
pub async fn cli(a: JoinArgs, shutdown: Shutdown) -> anyhow::Result<Option<crate::agent::Opts>> {
    let interactive = interactive();
    let state_dir = crate::config::default_data_dir().join("agent");
    if enrolled(&state_dir) && !a.yes {
        let other = stored(&state_dir, "server").unwrap_or_else(|| "a kiln server".to_owned());
        anyhow::ensure!(
            interactive,
            "this machine is already an agent of {other} (credentials in {}); pass --yes to replace that",
            state_dir.display()
        );
        let answer = ask(&format!("This machine is already an agent of {other}. Replace that enrollment? [y/N] "), &shutdown).await?;
        anyhow::ensure!(yes(&answer), "not joining");
    }
    let server = match a.server.clone() {
        Some(s) => s,
        None => {
            say_stderr("Looking for kiln servers on this network…");
            let found = find_servers(Duration::from_secs(a.wait.clamp(1, 60)), &[]).await?;
            choose(found, a.yes, interactive, &shutdown).await?
        }
    };
    let me = Machine::this(a.name.clone(), &a.labels);
    let joined = request(&server, &me, &state_dir, &say_stderr, &shutdown).await?;
    if a.no_install {
        say_stderr("Starting the agent in the foreground (Ctrl-C stops it).");
        let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        return Ok(Some(crate::agent::Opts {
            server_url: joined.server,
            quic: Some(joined.quic),
            name: joined.name,
            labels: joined.labels,
            slots: a.slots.unwrap_or_else(|| u32::try_from((cpus / 4).max(1)).unwrap_or(1)),
            work_dir: state_dir.join("work"),
            state_dir,
            join_token: None,
            ephemeral: false,
            dedicated_job: None,
            terminal: true,
            shell: None,
            credentials: None,
            instance: None,
        }));
    }
    crate::install::install(crate::install::InstallArgs {
        unattended: true,
        system: false,
        agent: true,
        boot: a.boot,
        no_start: false,
        server: Some(joined.server.clone()),
        quic: Some(joined.quic),
        token: None,
        name: Some(joined.name.clone()),
        labels: joined.labels,
        slots: a.slots,
        upgrade: false,
        service: "kiln-agent".to_owned(),
    })
    .await?;
    say_stderr(&format!("\nJoined: this machine is agent \"{}\" of {}. It starts on boot.", joined.name, joined.server));
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn csr() -> anyhow::Result<(String, String)> {
        crate::pki::make_csr("Bench Box")
    }

    fn new_request(csr: &str, secret: &str) -> NewRequest {
        NewRequest {
            name: "Bench Box".into(),
            hostname: "bench-box".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            labels: vec![],
            version: "0.1.0".into(),
            csr: csr.into(),
            commitment: crate::util::sha256_hex(secret.as_bytes()),
        }
    }

    fn ip(n: u8) -> IpAddr {
        IpAddr::from([192, 168, 1, n])
    }

    #[test]
    fn verification_codes() -> anyhow::Result<()> {
        let (csr, _) = csr()?;
        let pk = csr_public_key(&csr)?;
        let code = verification_code(&pk, b"ca", b"secret", b"nonce");
        assert_eq!(code.len(), 6);
        assert!(code.bytes().all(|b| b.is_ascii_digit()));
        assert_eq!(verification_code(&pk, b"ca", b"secret", b"nonce"), code, "deterministic");
        // Every input matters.
        let (other, _) = crate::pki::make_csr("Bench Box")?;
        let changed = [
            verification_code(&csr_public_key(&other)?, b"ca", b"secret", b"nonce"),
            verification_code(&pk, b"cb", b"secret", b"nonce"),
            verification_code(&pk, b"ca", b"secreu", b"nonce"),
            verification_code(&pk, b"ca", b"secret", b"noncf"),
            // Length prefixes: moving a byte between fields changes the code.
            verification_code(&pk, b"cas", b"ecret", b"nonce"),
        ];
        assert!(changed.iter().filter(|c| **c == code).count() == 0, "{code} {changed:?}");
        assert_eq!(display_code("012345"), "012 345");
        assert_eq!(display_code("12"), "12");
        // A CSR's key is read only after its signature checks out.
        assert!(csr_public_key("garbage").is_err());
        let tampered = csr.replacen("MI", "MJ", 1);
        assert!(csr_public_key(&tampered).is_err());
        Ok(())
    }

    #[test]
    fn requests_live_through_confirm_and_approve() -> anyhow::Result<()> {
        let r = Requests::default();
        let now = Instant::now();
        let ca = b"ca-der";
        let (csr, _) = csr()?;
        let (id, nonce) = r.create(new_request(&csr, "s3cret"), ip(10), now).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        // Not confirmed yet: no code, cannot be approved.
        let listed = r.list(now, ca);
        let first = listed.first().ok_or_else(|| anyhow::anyhow!("not listed"))?;
        assert_eq!((first.name.as_str(), first.code.clone(), first.status), ("bench-box", None, Status::Pending));
        assert_eq!(first.labels, vec!["baremetal", "linux", "x86_64"]);
        assert_eq!(first.address, "192.168.1.10");
        assert_eq!(r.candidate(&id, now).map(|_| ()), Err(Refusal::Unconfirmed));
        // The wrong secret learns nothing and confirms nothing.
        assert_eq!(r.poll(&id, "guess", now, ca).map(|_| ()), Err(Refusal::WrongSecret));
        assert_eq!(r.poll("nope", "s3cret", now, ca).map(|_| ()), Err(Refusal::Unknown));
        // The machine's first poll confirms it; both sides now compute the same code.
        let (p, confirmed) = r.poll(&id, "s3cret", now, ca).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        assert!(confirmed);
        let expected = verification_code(&csr_public_key(&csr)?, ca, b"s3cret", &nonce);
        assert_eq!(p.code.as_deref(), Some(expected.as_str()));
        assert_eq!(r.list(now, ca).first().and_then(|l| l.code.clone()), Some(expected.clone()));
        let (_, again) = r.poll(&id, "s3cret", now, ca).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        assert!(!again);
        // Approval: the admin signs the CSR; the machine receives the certificate.
        let c = r.candidate(&id, now).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let dir = tempfile::tempdir()?;
        let pki = crate::pki::Pki::open(dir.path())?;
        let cert = pki.sign_csr(&c.csr, &c.name, 365)?;
        let approval = Approval { name: c.name, labels: c.labels, cert_pem: cert, ca_pem: pki.ca_pem.clone(), quic: "box:7071".into(), quic_port: 7071 };
        r.approve(&id, approval, now).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let (p, _) = r.poll(&id, "s3cret", now, ca).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        assert_eq!(p.status, Status::Approved);
        let a = p.approval.ok_or_else(|| anyhow::anyhow!("no approval"))?;
        assert_eq!(cert_key_bits(&a.cert_pem), csr_key_bits(&csr));
        assert_eq!(crate::pki::cert_dns_name(&pem_der(&a.cert_pem)?).as_deref(), Some("bench-box"));
        // Decided: no second decision.
        assert_eq!(r.deny(&id, now), Err(Refusal::NotPending(Status::Approved)));
        assert_eq!(r.list(now, ca).first().and_then(|l| l.agent.clone()).as_deref(), Some("bench-box"));
        Ok(())
    }

    #[test]
    fn requests_are_denied_and_expire() -> anyhow::Result<()> {
        let r = Requests::default();
        let now = Instant::now();
        let (csr, _) = csr()?;
        let (id, _) = r.create(new_request(&csr, "x"), ip(11), now).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        assert_eq!(r.deny(&id, now), Ok("bench-box".to_owned()));
        let (p, _) = r.poll(&id, "x", now, b"ca").map_err(|e| anyhow::anyhow!("{e:?}"))?;
        assert_eq!((p.status, p.approval.is_none()), (Status::Denied, true));
        assert_eq!(r.candidate(&id, now).map(|_| ()), Err(Refusal::NotPending(Status::Denied)));
        // Ten minutes later it is gone.
        let later = now + REQUEST_TTL + Duration::from_secs(1);
        assert_eq!(r.poll(&id, "x", later, b"ca").map(|_| ()), Err(Refusal::Unknown));
        assert!(r.list(later, b"ca").is_empty());
        let (id2, _) = r.create(new_request(&csr, "y"), ip(11), later).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let near = later + REQUEST_TTL - Duration::from_secs(1);
        assert!(r.poll(&id2, "y", near, b"ca").is_ok_and(|(p, _)| p.expires_in <= 1));
        Ok(())
    }

    #[test]
    fn requests_are_rate_limited_and_validated() -> anyhow::Result<()> {
        let r = Requests::default();
        let now = Instant::now();
        let (csr, _) = csr()?;
        assert!(r.create(new_request(&csr, "a"), ip(1), now).is_ok());
        // The same address again within 5 s is refused, with when to retry.
        match r.create(new_request(&csr, "b"), ip(1), now + Duration::from_secs(2)) {
            Err(Refusal::TooMany { retry_after, .. }) => assert_eq!(retry_after, Duration::from_secs(3)),
            other => anyhow::bail!("expected a rate limit, got {other:?}"),
        }
        assert!(r.create(new_request(&csr, "b"), ip(1), now + PER_SOURCE).is_ok());
        // At most 20 waiting.
        for n in 2..=19u8 {
            assert!(r.create(new_request(&csr, "c"), ip(n), now).is_ok(), "request {n}");
        }
        assert_eq!(r.pending(now), MAX_PENDING);
        assert!(matches!(r.create(new_request(&csr, "d"), ip(30), now), Err(Refusal::TooMany { .. })));
        // A decision makes room.
        let id = r.list(now, b"ca").first().map(|l| l.id.clone()).unwrap_or_default();
        r.deny(&id, now).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        assert!(r.create(new_request(&csr, "d"), ip(30), now).is_ok());
        // Malformed requests.
        let r = Requests::default();
        let mut bad = new_request("not a csr", "e");
        assert!(matches!(r.create(bad.clone(), ip(40), now), Err(Refusal::Invalid(_))));
        bad.csr = "x".repeat(CSR_LIMIT + 1);
        assert!(matches!(r.create(bad, ip(41), now), Err(Refusal::Invalid(_))));
        let mut bad = new_request(&csr, "e");
        bad.commitment = "abc".into();
        assert!(matches!(r.create(bad, ip(42), now), Err(Refusal::Invalid(_))));
        // Hostile names and labels are cleaned.
        let mut odd = new_request(&csr, "f");
        odd.name = "  ".into();
        odd.hostname = "Box\u{1b}[1m".into();
        odd.os = "Linux; rm -rf".into();
        odd.labels = vec!["GPU".into(), "gpu".into(), "a b".into(), "".into(), "x".repeat(80)];
        let (id, _) = r.create(odd, ip(43), now).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let l = r.get(&id, now, b"ca").map_err(|e| anyhow::anyhow!("{e:?}"))?;
        assert_eq!(l.name, "box-1m");
        assert_eq!(l.os, "linuxrm-rf");
        assert_eq!(l.labels.first().map(String::as_str), Some("gpu"));
        assert_eq!(l.labels.get(1).map(String::as_str), Some("a-b"));
        assert_eq!(l.labels.len(), 3);
        assert!(l.labels.iter().all(|x| x.len() <= 63));
        Ok(())
    }

    #[test]
    fn urls_and_quic_addresses() -> anyhow::Result<()> {
        assert_eq!(host_of("http://192.168.1.7:7070").as_deref(), Some("192.168.1.7"));
        assert_eq!(host_of("http://[fe80::1]:7070/").as_deref(), Some("fe80::1"));
        assert_eq!(host_of("https://ci.example").as_deref(), Some("ci.example"));
        crate::rt::block_on(async {
            let a = Approval { name: "n".into(), labels: vec![], cert_pem: String::new(), ca_pem: String::new(), quic: "srv-host:7071".into(), quic_port: 7071 };
            // An IP the machine reached HTTP at beats an advertised host name.
            assert_eq!(quic_address("http://192.168.1.7:7070", &a).await, "192.168.1.7:7071");
            assert_eq!(quic_address("http://[fe80::1]:7070", &a).await, "[fe80::1]:7071");
            // The same host: as advertised.
            assert_eq!(quic_address("http://srv-host:7070", &a).await, "srv-host:7071");
            // An advertised name that resolves here is kept; one that does not falls back.
            let local = Approval { quic: "localhost:7999".into(), ..a.clone() };
            assert_eq!(quic_address("http://kiln.invalid:7070", &local).await, "localhost:7999");
            let bogus = Approval { quic: "no-such-host.invalid:7071".into(), ..a };
            assert_eq!(quic_address("http://box.local:7070", &bogus).await, "box.local:7071");
            Ok(())
        })
    }
}
