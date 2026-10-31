//! The end-to-end encryption between two runners (`docs/reach.md` §3).
//!
//! Every runner has a reach identity of its own: a P-384 key generated on the machine, which never
//! leaves it, in a self-signed certificate naming the runner. Its fingerprint is the SHA-256 of
//! the key's SubjectPublicKeyInfo, so it survives a new certificate for the same key.
//!
//! Two runners talk TLS 1.3 to each other through the server's relay (`super::relay`), so the
//! server forwards ciphertext it cannot read or alter:
//!
//! - key exchange: the hybrid post-quantum groups only (ML-KEM-1024 with ECDH P-384 first, then
//!   ML-KEM-768 with X25519), hyper-raft's node groups;
//! - records: AES-256-GCM only, with a key per direction;
//! - authentication: both sides present their identity certificate and each side accepts only the
//!   key it expects for that runner: the one it pinned when they first met (trust on first use),
//!   else the one the server's directory lists, which it then pins. A server that later hands out a
//!   different key for a runner is refused, never believed.
//!
//! No session tickets, no 0-RTT: every link is a full handshake with fresh keys.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use anyhow::Context as _;
use hyper_quic::rustls::{self, DigitallySignedStruct, DistinguishedName, SignatureScheme};
use hyper_quic::rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use hyper_quic::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use hyper_quic::rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// The ALPN both ends require: a link speaks reach's inner protocol, nothing else.
const ALPN: &[u8] = b"kiln-reach/1";

/// The provider for links: hyper-raft's node provider narrowed to AES-256-GCM.
static PROVIDER: LazyLock<rustls::crypto::CryptoProvider> = LazyLock::new(|| rustls::crypto::CryptoProvider {
    cipher_suites: vec![rustls::crypto::aws_lc_rs::cipher_suite::TLS13_AES_256_GCM_SHA384],
    kx_groups: vec![rustls::crypto::aws_lc_rs::kx_group::SECP384R1MLKEM1024, rustls::crypto::aws_lc_rs::kx_group::X25519MLKEM768],
    ..hyper_quic::crypto::rustls::node_provider().clone()
});

/// SHA-256 of a SubjectPublicKeyInfo (DER), hex: a reach identity's fingerprint.
pub fn fingerprint_of_spki(spki: &[u8]) -> String {
    crate::util::sha256_hex(spki)
}

/// The fingerprint of the key in a certificate (DER).
pub fn fingerprint_of_cert(der: &[u8]) -> Option<String> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).ok()?;
    Some(fingerprint_of_spki(cert.public_key().raw))
}

/// A fingerprint as people compare it: the first 16 bytes in groups of four hex digits.
pub fn short(fingerprint: &str) -> String {
    let head: Vec<char> = fingerprint.chars().take(32).collect();
    head.chunks(4).map(|c| c.iter().collect::<String>()).collect::<Vec<_>>().join(" ")
}

/// This runner's reach identity.
pub struct Identity {
    pub name: String,
    pub cert_der: Vec<u8>,
    key_der: Vec<u8>,
    pub fingerprint: String,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity").field("name", &self.name).field("fingerprint", &self.fingerprint).field("key", &"<redacted>").finish()
    }
}

impl Identity {
    /// The identity kept in `dir` (`identity.key`, `0600`, and `identity.crt`), made on first use.
    /// A certificate naming another runner (the machine was renamed) is reissued for the same key.
    pub fn load_or_create(dir: &Path, name: &str) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let key_path = dir.join("identity.key");
        let cert_path = dir.join("identity.crt");
        let key = match std::fs::read_to_string(&key_path) {
            Ok(pem) => rcgen::KeyPair::from_pem(&pem).context("read the reach identity key")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).context("generate the reach identity key")?;
                crate::util::write_secret(&key_path, key.serialize_pem().as_bytes(), 0o600)?;
                let _ = std::fs::remove_file(&cert_path);
                key
            }
            Err(e) => return Err(e).with_context(|| format!("read {}", key_path.display())),
        };
        let spki = rcgen::PublicKeyData::subject_public_key_info(&key);
        let fingerprint = fingerprint_of_spki(&spki);
        let kept = std::fs::read(&cert_path).ok().filter(|der| crate::pki::cert_dns_name(der).as_deref() == Some(name) && fingerprint_of_cert(der).as_deref() == Some(fingerprint.as_str()));
        let cert_der = match kept {
            Some(der) => der,
            None => {
                let der = self_signed(&key, name)?;
                std::fs::write(&cert_path, &der).with_context(|| format!("write {}", cert_path.display()))?;
                der
            }
        };
        Ok(Self { name: name.to_owned(), cert_der, key_der: key.serialize_der(), fingerprint })
    }

    fn chain(&self) -> Vec<CertificateDer<'static>> {
        vec![CertificateDer::from(self.cert_der.clone())]
    }

    fn key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_der.clone()))
    }
}

/// A long-lived certificate for the identity key: the key is what peers pin, not the dates.
fn self_signed(key: &rcgen::KeyPair, name: &str) -> anyhow::Result<Vec<u8>> {
    let mut p = rcgen::CertificateParams::new(vec![name.to_owned()]).context("reach identity name")?;
    let now = time::OffsetDateTime::now_utc();
    p.not_before = now - time::Duration::hours(1);
    p.not_after = now + time::Duration::days(365 * 30);
    p.distinguished_name.push(rcgen::DnType::CommonName, format!("{name} (kiln reach)"));
    p.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
    p.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth, rcgen::ExtendedKeyUsagePurpose::ClientAuth];
    Ok(p.self_signed(key).context("self-sign the reach identity")?.der().to_vec())
}

/// One runner's pinned key.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Pin {
    pub fingerprint: String,
    /// Unix seconds it was first seen.
    pub since: i64,
}

/// What a pin store says about a key a runner presents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trust {
    /// The pinned key.
    Known,
    /// No pin yet: this key is pinned now (trust on first use).
    First,
    /// Another key than the pinned one.
    Changed { pinned: String },
}

/// The keys this runner pinned, by runner name (`pins.json` beside the identity).
pub struct Pins {
    path: Option<PathBuf>,
    pins: Mutex<PinState>,
}

enum PinState {
    Verified(BTreeMap<String, Pin>),
    Uncertain(String),
    ReleaseFailed { why: String, _disk: std::fs::File },
}

struct LockedPins<'a> {
    state: parking_lot::MutexGuard<'a, PinState>,
    disk: Option<std::fs::File>,
}

impl LockedPins<'_> {
    fn release(&mut self) -> anyhow::Result<()> {
        self.release_with(std::fs::File::unlock)
    }

    fn release_with(&mut self, unlock: impl FnOnce(&std::fs::File) -> std::io::Result<()>) -> anyhow::Result<()> {
        let Some(file) = self.disk.take() else { return Ok(()) };
        // U09/U17: close alone leaves a fork's duplicate holding the physical lock.
        // Unlock before RAM ownership ends, including when canonical reload failed.
        if let Err(error) = unlock(&file) {
            let prior = self.state.verified().err().map(|error| format!("; previous refusal: {error:#}")).unwrap_or_default();
            let why = format!("reach pin lock release failed: {error}{prior}; ownership is retained and this instance refuses further access");
            *self.state = PinState::ReleaseFailed { why: why.clone(), _disk: file };
            return Err(error).context(why);
        }
        Ok(())
    }

    fn finish<T>(mut self, result: anyhow::Result<T>) -> anyhow::Result<T> {
        if let Err(error) = self.release() {
            return match result {
                Ok(_) => Err(error),
                Err(prior) => Err(error).with_context(|| format!("reach pin access also failed: {prior:#}")),
            };
        }
        result
    }
}

impl Drop for LockedPins<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.release() {
            // Unwinding/early errors cannot return a second error; retain the descriptor
            // in the fenced state and report it instead of claiming a successful release.
            tracing::error!(error = %error, "reach pin lock ownership retained");
        }
    }
}

impl PinState {
    fn verified(&self) -> anyhow::Result<&BTreeMap<String, Pin>> {
        match self {
            Self::Verified(pins) => Ok(pins),
            Self::Uncertain(why) => Err(anyhow::anyhow!(why.clone())),
            Self::ReleaseFailed { why, .. } => Err(anyhow::anyhow!(why.clone())),
        }
    }
}

impl Pins {
    pub fn open(dir: &Path) -> anyhow::Result<Self> {
        let pins = Self { path: Some(dir.join("pins.json")), pins: Mutex::new(PinState::Verified(BTreeMap::new())) };
        pins.locked()?.finish(Ok(()))?;
        Ok(pins)
    }

    fn load(path: &Path) -> anyhow::Result<BTreeMap<String, Pin>> {
        // U09/U17: an unreadable or corrupt trust anchor is never a new TOFU store.
        let pins = match std::fs::read(path) {
            Ok(bytes) => {
                let pins = serde_json::from_slice(&bytes).with_context(|| format!("decode reach pins {}", path.display()))?;
                // Reloading verifies the persisted map's durability too. An uncertain
                // instance is fenced before reaching this path; only explicit open resets it.
                #[cfg(unix)]
                std::fs::File::open(path).and_then(|file| file.sync_all()).with_context(|| format!("sync reach pins {}", path.display()))?;
                Self::sync_directory(path)?;
                pins
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e).with_context(|| format!("read reach pins {}", path.display())),
        };
        Ok(pins)
    }

    /// U09/U17: one stable physical lock serializes only this access, across overlapping
    /// runners too. The canonical bootstrap map is reloaded before any trust decision.
    fn locked(&self) -> anyhow::Result<LockedPins<'_>> {
        let state = self.pins.try_lock().ok_or_else(Self::busy)?;
        state.verified()?;
        let mut locked = LockedPins { state, disk: None };
        if let Some(path) = &self.path {
            let parent = path.parent().context("reach pins have no parent directory")?;
            std::fs::create_dir_all(parent).with_context(|| format!("create reach pins directory {}", parent.display()))?;
            let lock_path = parent.join(".pins.lock");
            let file = crate::os::with_mode(std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false), 0o600)
                .open(&lock_path).with_context(|| format!("open reach pins lock {}", lock_path.display()))?;
            file.try_lock().map_err(|error| match error {
                std::fs::TryLockError::WouldBlock => Self::busy(),
                std::fs::TryLockError::Error(error) => error,
            }).with_context(|| format!("lock reach pins {}", lock_path.display()))?;
            locked.disk = Some(file);
            match Self::load(path) {
                Ok(pins) => *locked.state = PinState::Verified(pins),
                Err(error) => return locked.finish(Err(error)),
            }
        }
        Ok(locked)
    }

    fn busy() -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::WouldBlock, "reach pin store is busy; retry after its current access completes")
    }

    /// U09/U17: bootstrap filesystem work runs on the existing bounded blocking share.
    /// The shared owner outlives cancellation; typed storage and busy errors pass through.
    pub async fn access<T, F>(self: &Arc<Self>, work: F) -> anyhow::Result<T>
    where T: Send + 'static, F: FnOnce(&Self) -> anyhow::Result<T> + Send + 'static {
        let pins = Arc::clone(self);
        crate::rt::blocking(move || work(&pins)).await?
    }

    /// Pins kept in memory only (tests).
    pub fn memory() -> Self {
        Self { path: None, pins: Mutex::new(PinState::Verified(BTreeMap::new())) }
    }

    pub fn get(&self, name: &str) -> anyhow::Result<Option<Pin>> {
        let locked = self.locked()?;
        let result = locked.state.verified().map(|pins| pins.get(name).cloned());
        locked.finish(result)
    }

    pub fn all(&self) -> anyhow::Result<BTreeMap<String, Pin>> {
        let locked = self.locked()?;
        let result = locked.state.verified().cloned();
        locked.finish(result)
    }

    /// Check `fingerprint` for `name`; an unknown runner's key is pinned (and saved).
    pub fn check(&self, name: &str, fingerprint: &str) -> anyhow::Result<Trust> {
        let mut locked = self.locked()?;
        let result = (|| {
            let state = &mut *locked.state;
            let pins = state.verified()?;
            match pins.get(name) {
                Some(p) if p.fingerprint == fingerprint => Ok(Trust::Known),
                Some(p) => Ok(Trust::Changed { pinned: p.fingerprint.clone() }),
                None => {
                    let mut next = pins.clone();
                    next.insert(name.to_owned(), Pin { fingerprint: fingerprint.to_owned(), since: crate::util::now_unix() });
                    self.save(state, next)?;
                    Ok(Trust::First)
                }
            }
        })();
        locked.finish(result)
    }

    /// Forget `name`'s key (it was reinstalled): the next one it presents is pinned.
    pub fn forget(&self, name: &str) -> anyhow::Result<bool> {
        let mut locked = self.locked()?;
        let result = (|| {
            let state = &mut *locked.state;
            let pins = state.verified()?;
            if !pins.contains_key(name) { return Ok(false) }
            let mut next = pins.clone();
            next.remove(name);
            self.save(state, next)?;
            Ok(true)
        })();
        locked.finish(result)
    }

    fn save(&self, state: &mut PinState, pins: BTreeMap<String, Pin>) -> anyhow::Result<()> {
        self.save_with_directory_sync(state, pins, Self::sync_directory)
    }

    fn save_with_directory_sync(&self, state: &mut PinState, pins: BTreeMap<String, Pin>, sync: impl FnOnce(&Path) -> anyhow::Result<()>) -> anyhow::Result<()> {
        state.verified()?;
        if let Some(path) = &self.path {
            let bytes = serde_json::to_vec_pretty(&pins).context("encode reach pins")?;
            crate::util::write_secret(path, &bytes, 0o600).with_context(|| format!("persist reach pins {}", path.display()))?;
            if let Err(error) = sync(path) {
                // U09/U17: rename already committed. Err cannot imply rollback; stale RAM
                // must never authorize trust or overwrite the now-unknown persisted map.
                let why = format!("reach pin storage is uncertain after rename: {error:#}; trust and mutations are fenced until the on-disk pins are verified and reopened");
                *state = PinState::Uncertain(why.clone());
                anyhow::bail!("{why}");
            }
        }
        *state = PinState::Verified(pins);
        Ok(())
    }

    fn sync_directory(path: &Path) -> anyhow::Result<()> {
        // U09/U17: the atomic rename must be durable before publishing a trust change.
        #[cfg(unix)]
        {
            let parent = path.parent().context("reach pins have no parent directory")?;
            std::fs::File::open(parent).and_then(|dir| dir.sync_all()).with_context(|| format!("sync reach pins directory {}", parent.display()))?;
        }
        #[cfg(not(unix))]
        let _ = path;
        Ok(())
    }
}

/// Accepts exactly one key: the peer's expected fingerprint. Serves both directions (a link's
/// client checks the server's certificate, its server the client's).
#[derive(Debug)]
struct Expect {
    fingerprint: String,
}

impl Expect {
    fn check(&self, end_entity: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        match fingerprint_of_cert(end_entity) {
            Some(f) if f == self.fingerprint => Ok(()),
            Some(_) => Err(rustls::Error::InvalidCertificate(rustls::CertificateError::ApplicationVerificationFailure)),
            None => Err(rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding)),
        }
    }

    fn signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &PROVIDER.signature_verification_algorithms)
    }
}

impl ServerCertVerifier for Expect {
    fn verify_server_cert(&self, end_entity: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: &ServerName<'_>, _: &[u8], _: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity).map(|()| ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(rustls::PeerIncompatible::Tls13RequiredForQuic))
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        PROVIDER.signature_verification_algorithms.supported_schemes()
    }
}

impl ClientCertVerifier for Expect {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(&self, end_entity: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: UnixTime) -> Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity).map(|()| ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(rustls::PeerIncompatible::Tls13RequiredForQuic))
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        PROVIDER.signature_verification_algorithms.supported_schemes()
    }
}

/// hyper-tls keeps a connection's configuration with its owner: each side holds both.
enum Conn {
    Client(rustls::ClientConnection, Box<rustls::ClientConfig>),
    Server(rustls::ServerConnection, Box<rustls::ServerConfig>),
}

macro_rules! with_conn {
    ($c:expr, $x:ident => $body:expr) => {
        match $c {
            Conn::Client($x, _) => $body,
            Conn::Server($x, _) => $body,
        }
    };
}

/// One end of an encrypted link: ciphertext in and out, plaintext in and out. Sans-io: the link
/// moves the bytes (`super::link`).
pub struct Tls {
    conn: Conn,
    closed: bool,
}

/// What a link negotiated, for logs and status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Negotiated {
    pub suite: String,
    pub group: String,
}

impl Tls {
    /// The side that opens a link: proves `me`, accepts only `peer_fingerprint`.
    pub fn client(me: &Identity, peer_fingerprint: &str) -> anyhow::Result<Self> {
        let verifier = Box::new(Expect { fingerprint: peer_fingerprint.to_owned() });
        let mut cfg = rustls::ClientConfig::builder_with_provider(&PROVIDER)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .context("reach TLS versions")?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_client_auth_cert(me.chain(), me.key())
            .context("reach client identity")?;
        cfg.alpn_protocols = vec![ALPN.to_vec()];
        cfg.enable_sni = false;
        cfg.resumption = rustls::client::Resumption::disabled();
        cfg.enable_early_data = false;
        // The name is not checked (the verifier pins the key); SNI is off, so it is never sent.
        let name = ServerName::try_from("kiln-reach").context("reach server name")?;
        let mut conn = rustls::ClientConnection::new(&mut cfg, name).context("reach TLS client")?;
        // rustls refuses writes past 64 KiB of unsent data; a link's windows bound it instead (a
        // message is up to 4 MiB).
        conn.set_buffer_limit(None);
        Ok(Self { conn: Conn::Client(conn, Box::new(cfg)), closed: false })
    }

    /// The side a link arrives at: proves `me`, accepts only `peer_fingerprint`.
    pub fn server(me: &Identity, peer_fingerprint: &str) -> anyhow::Result<Self> {
        let verifier = Box::new(Expect { fingerprint: peer_fingerprint.to_owned() });
        let mut cfg = rustls::ServerConfig::builder_with_provider(&PROVIDER)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .context("reach TLS versions")?
            .with_client_cert_verifier(verifier)
            .with_single_cert(me.chain(), me.key())
            .context("reach server identity")?;
        cfg.alpn_protocols = vec![ALPN.to_vec()];
        cfg.send_tls13_tickets = 0;
        cfg.max_early_data_size = 0;
        let mut conn = rustls::ServerConnection::new(&cfg).context("reach TLS server")?;
        conn.set_buffer_limit(None);
        Ok(Self { conn: Conn::Server(conn, Box::new(cfg)), closed: false })
    }

    /// Ciphertext from the peer: the plaintext it carried is appended to `plain`. An error (a bad
    /// record, a refused certificate) ends the link; [`Tls::outgoing`] still holds the alert.
    pub fn receive(&mut self, mut ciphertext: &[u8], plain: &mut Vec<u8>) -> anyhow::Result<()> {
        while !ciphertext.is_empty() {
            let n = with_conn!(&mut self.conn, c => c.read_tls(&mut ciphertext)).context("reach TLS read")?;
            if n == 0 {
                break;
            }
            let processed = match &mut self.conn {
                Conn::Client(c, cfg) => c.process_new_packets(cfg).map(|_| ()),
                Conn::Server(c, cfg) => c.process_new_packets(cfg).map(|_| ()),
            };
            processed.map_err(|e| anyhow::anyhow!("reach TLS: {e}"))?;
            self.drain(plain)?;
        }
        Ok(())
    }

    fn drain(&mut self, plain: &mut Vec<u8>) -> anyhow::Result<()> {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match with_conn!(&mut self.conn, c => c.reader().read(&mut buf)) {
                Ok(0) => {
                    self.closed = true;
                    return Ok(());
                }
                Ok(n) => plain.extend_from_slice(buf.get(..n).unwrap_or_default()),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    self.closed = true;
                    return Ok(());
                }
                Err(e) => return Err(e).context("reach TLS plaintext"),
            }
        }
    }

    /// Plaintext for the peer (held until the handshake is done, then encrypted).
    pub fn send(&mut self, plain: &[u8]) -> anyhow::Result<()> {
        with_conn!(&mut self.conn, c => c.writer().write_all(plain)).context("reach TLS write")
    }

    /// Ciphertext waiting to go to the peer.
    pub fn outgoing(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        while with_conn!(&self.conn, c => c.wants_write()) {
            match with_conn!(&mut self.conn, c => c.write_tls(&mut out)) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        out
    }

    /// Say goodbye (close_notify); [`Tls::outgoing`] then holds it.
    pub fn close(&mut self) {
        with_conn!(&mut self.conn, c => c.send_close_notify());
    }

    /// The handshake is done: both keys proved, traffic keys in place.
    pub fn ready(&self) -> bool {
        !with_conn!(&self.conn, c => c.is_handshaking())
    }

    /// The peer said goodbye.
    pub fn closed(&self) -> bool {
        self.closed
    }

    pub fn negotiated(&self) -> Option<Negotiated> {
        let (suite, group) = with_conn!(&self.conn, c => (c.negotiated_cipher_suite(), c.negotiated_key_exchange_group()));
        Some(Negotiated { suite: format!("{:?}", suite?.suite()), group: format!("{:?}", group?.name()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Move ciphertext back and forth until neither side has anything to say.
    fn pump(a: &mut Tls, b: &mut Tls, a_got: &mut Vec<u8>, b_got: &mut Vec<u8>) -> anyhow::Result<()> {
        for _ in 0..32 {
            let to_b = a.outgoing();
            let to_a = b.outgoing();
            if to_b.is_empty() && to_a.is_empty() {
                return Ok(());
            }
            b.receive(&to_b, b_got)?;
            a.receive(&to_a, a_got)?;
        }
        anyhow::bail!("the handshake never settled")
    }

    #[test]
    fn identities_persist_and_rename_keeps_the_key() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let a = Identity::load_or_create(dir.path(), "mac")?;
        let again = Identity::load_or_create(dir.path(), "mac")?;
        assert_eq!(a.fingerprint, again.fingerprint);
        assert_eq!(a.cert_der, again.cert_der);
        let renamed = Identity::load_or_create(dir.path(), "mac-2")?;
        assert_eq!(renamed.fingerprint, a.fingerprint, "a new name keeps the key");
        assert_eq!(crate::pki::cert_dns_name(&renamed.cert_der).as_deref(), Some("mac-2"));
        assert_eq!(fingerprint_of_cert(&a.cert_der).as_deref(), Some(a.fingerprint.as_str()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(std::fs::metadata(dir.path().join("identity.key"))?.permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(short("0123456789abcdef0123456789abcdef00"), "0123 4567 89ab cdef 0123 4567 89ab cdef");
        Ok(())
    }

    #[test]
    fn a_link_is_post_quantum_aes_gcm_both_ways() -> anyhow::Result<()> {
        let (da, db) = (tempfile::tempdir()?, tempfile::tempdir()?);
        let (a, b) = (Identity::load_or_create(da.path(), "mac")?, Identity::load_or_create(db.path(), "linux")?);
        let mut client = Tls::client(&a, &b.fingerprint)?;
        let mut server = Tls::server(&b, &a.fingerprint)?;
        // Written before the handshake finished: held, then sent encrypted.
        client.send(b"hello linux")?;
        let (mut at_a, mut at_b) = (Vec::new(), Vec::new());
        pump(&mut client, &mut server, &mut at_a, &mut at_b)?;
        assert!(client.ready() && server.ready());
        assert_eq!(at_b, b"hello linux");
        server.send(b"hello mac")?;
        pump(&mut client, &mut server, &mut at_a, &mut at_b)?;
        assert_eq!(at_a, b"hello mac");
        let n = client.negotiated().ok_or_else(|| anyhow::anyhow!("nothing negotiated"))?;
        assert_eq!(n.suite, "TLS13_AES_256_GCM_SHA384");
        assert_eq!(n.group, "secp384r1MLKEM1024");
        assert_eq!(server.negotiated(), Some(n));
        // A message far over rustls's default 64 KiB buffer goes in one send.
        let big = vec![7u8; 3 << 20];
        server.send(&big)?;
        at_a.clear();
        pump(&mut client, &mut server, &mut at_a, &mut at_b)?;
        assert_eq!(at_a.len(), big.len());
        // Goodbye reaches the other side.
        client.close();
        pump(&mut client, &mut server, &mut at_a, &mut at_b)?;
        assert!(server.closed());
        Ok(())
    }

    #[test]
    fn a_key_other_than_the_expected_one_is_refused_both_ways() -> anyhow::Result<()> {
        let dirs = [tempfile::tempdir()?, tempfile::tempdir()?, tempfile::tempdir()?];
        let [da, db, dm] = &dirs;
        let a = Identity::load_or_create(da.path(), "mac")?;
        let b = Identity::load_or_create(db.path(), "linux")?;
        // Someone in the middle with its own key, claiming to be linux.
        let mallory = Identity::load_or_create(dm.path(), "linux")?;
        let mut client = Tls::client(&a, &b.fingerprint)?;
        let mut fake = Tls::server(&mallory, &a.fingerprint)?;
        let (mut x, mut y) = (Vec::new(), Vec::new());
        client.send(b"secret")?;
        let failed = pump(&mut client, &mut fake, &mut x, &mut y);
        assert!(failed.is_err(), "the client must refuse a key it does not expect");
        assert!(y.is_empty(), "nothing reached the impostor");
        // And a server refuses a client presenting another key than the one it expects.
        let mut impostor = Tls::client(&mallory, &b.fingerprint)?;
        let mut server = Tls::server(&b, &a.fingerprint)?;
        impostor.send(b"run this")?;
        let failed = pump(&mut impostor, &mut server, &mut x, &mut y);
        assert!(failed.is_err(), "the server must refuse a key it does not expect");
        assert!(y.is_empty());
        Ok(())
    }

    #[test]
    fn u09_u17_pins_trust_on_first_use_then_refuse_a_change() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pins = Pins::open(dir.path())?;
        assert_eq!(pins.check("linux", "aa")?, Trust::First);
        assert_eq!(pins.check("linux", "aa")?, Trust::Known);
        assert_eq!(pins.check("linux", "bb")?, Trust::Changed { pinned: "aa".into() });
        // Kept on disk.
        let path = dir.path().join("pins.json");
        let kept = std::fs::read(&path)?;
        // Refuse writes portably, including when tests run as root: block the atomic temp path.
        let blocked = path.with_extension(format!("tmp.{}", std::process::id()));
        std::fs::create_dir(&blocked)?;
        assert!(pins.check("mac", "cc").is_err());
        assert!(pins.get("mac")?.is_none(), "failed first-use persistence must not publish trust");
        assert!(pins.forget("linux").is_err());
        assert_eq!(pins.check("linux", "aa")?, Trust::Known, "failed forgetting retains its anchor");
        assert_eq!(std::fs::read(&path)?, kept);
        let reopened = Pins::open(dir.path())?;
        assert_eq!(reopened.check("linux", "aa")?, Trust::Known);
        assert!(reopened.get("mac")?.is_none());
        std::fs::remove_dir(&blocked)?;
        assert!(reopened.forget("linux")?);
        assert!(!reopened.forget("linux")?);
        assert!(Pins::open(dir.path())?.get("linux")?.is_none(), "successful forgetting survives restart");
        assert_eq!(reopened.check("linux", "bb")?, Trust::First);
        assert_eq!(Pins::open(dir.path())?.check("linux", "bb")?, Trust::Known);
        // A post-rename sync error may already have removed the anchor from disk. Refuse
        // every subsequent trust decision and write until a verified reopen reads that map.
        let uncertain = Pins::open(dir.path())?;
        {
            let mut locked = uncertain.locked()?;
            let mut next = locked.state.verified()?.clone();
            next.remove("linux");
            assert!(uncertain.save_with_directory_sync(&mut locked.state, next, |_| anyhow::bail!("injected directory sync failure")).is_err());
        }
        let persisted = std::fs::read(&path)?;
        assert!(serde_json::from_slice::<BTreeMap<String, Pin>>(&persisted)?.is_empty(), "rename happened despite Err");
        assert!(uncertain.get("linux").is_err());
        assert!(uncertain.all().is_err());
        assert!(uncertain.check("linux", "bb").is_err());
        assert!(uncertain.check("mac", "cc").is_err());
        assert!(uncertain.forget("linux").is_err());
        assert_eq!(std::fs::read(&path)?, persisted, "fenced RAM cannot overwrite the persisted outcome");
        assert_eq!(Pins::open(dir.path())?.check("linux", "bb")?, Trust::First, "verified reopen reads the committed map");
        std::fs::write(&path, b"{corrupt pins")?;
        assert!(Pins::open(dir.path()).is_err(), "a corrupt anchor must not become an empty store");
        assert_eq!(std::fs::read(&path)?, b"{corrupt pins");
        std::fs::remove_file(&path)?;
        std::fs::create_dir(&path)?;
        assert!(Pins::open(dir.path()).is_err(), "an unreadable existing anchor must not become an empty store");
        Ok(())
    }

    #[test]
    fn u09_u17_overlapping_pin_instances_share_current_anchors_and_short_lock() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let left = Pins::open(dir.path())?;
        let right = Pins::open(dir.path())?;
        let probe = std::fs::OpenOptions::new().read(true).write(true).open(dir.path().join(".pins.lock"))?;
        {
            let _held = left.locked()?;
            assert!(matches!(probe.try_lock(), Err(std::fs::TryLockError::WouldBlock)), "each access holds the common physical file lock");
            for pins in [&left, &right] {
                let error = pins.get("linux").err().context("contended pin access must refuse without waiting")?;
                assert_eq!(error.downcast_ref::<std::io::Error>().map(std::io::Error::kind), Some(std::io::ErrorKind::WouldBlock));
            }
        }
        probe.try_lock().map_err(|error| anyhow::anyhow!("access retained its lock: {error}"))?;
        probe.unlock()?;
        #[cfg(unix)]
        {
            let held = left.locked()?;
            // U09/U17: dup shares the description just as fork does, without spawning.
            let inherited = held.disk.as_ref().context("disk lock missing")?.try_clone()?;
            drop(held);
            probe.try_lock().map_err(|error| anyhow::anyhow!("duplicate retained the completed access lock: {error}"))?;
            probe.unlock()?;
            drop(inherited);
        }
        drop(probe);
        assert_eq!(left.check("linux", "aa")?, Trust::First);
        assert_eq!(right.check("linux", "bb")?, Trust::Changed { pinned: "aa".into() });
        assert_eq!(right.check("mac", "cc")?, Trust::First);
        assert_eq!(left.get("mac")?.map(|pin| pin.fingerprint), Some("cc".into()), "reads refresh the other instance's anchor");
        assert!(right.forget("linux")?);
        assert_eq!(left.check("windows", "dd")?, Trust::First);
        assert!(right.get("linux")?.is_none(), "a stale writer cannot resurrect a forgotten anchor");
        assert_eq!(right.get("windows")?.map(|pin| pin.fingerprint), Some("dd".into()));
        let anchors = Pins::open(dir.path())?.all()?;
        assert_eq!(anchors.len(), 2);
        assert_eq!(anchors.get("mac").map(|pin| pin.fingerprint.as_str()), Some("cc"));
        Ok(())
    }

    #[test]
    fn u09_u17_failed_pin_unlock_retains_ownership_and_refuses_access() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pins = Pins::open(dir.path())?;
        let probe = std::fs::File::options().read(true).write(true).open(dir.path().join(".pins.lock"))?;
        {
            let mut held = pins.locked()?;
            let error = held.release_with(|_| Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "injected unlock failure")))
                .err().context("unlock failure must be reported")?;
            assert_eq!(error.downcast_ref::<std::io::Error>().map(std::io::Error::kind), Some(std::io::ErrorKind::PermissionDenied));
        }
        assert!(matches!(probe.try_lock(), Err(std::fs::TryLockError::WouldBlock)), "failed unlock retains the owned descriptor");
        assert!(pins.get("linux").is_err());
        assert!(pins.check("linux", "aa").is_err());
        assert!(pins.forget("linux").is_err());
        Ok(())
    }

    #[test]
    fn u09_u17_async_pin_access_runs_off_shards_and_retains_typed_busy_errors() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let pins = Arc::new(Pins::memory());
            let trust = pins.access(|pins| {
                anyhow::ensure!(!crate::rt::on_shard(), "pin filesystem work must run off runtime shards");
                pins.check("linux", "aa")
            }).await?;
            assert_eq!(trust, Trust::First);
            let error = pins.access(|pins| {
                let _held = pins.locked()?;
                pins.get("linux")
            }).await.err().context("busy error from blocking adapter")?;
            assert_eq!(error.downcast_ref::<std::io::Error>().map(std::io::Error::kind), Some(std::io::ErrorKind::WouldBlock));
            Ok(())
        })
    }
}
