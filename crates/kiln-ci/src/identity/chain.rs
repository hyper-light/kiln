//! The chain of ownership (docs/ci.md W27-R01), checked purely: whom an actor's certificate says
//! it is, whom it acts for, and whether kiln's root stands behind it at a given time.
//!
//! - **An identity is one URI.** A leaf names its actor in exactly one URI subject alternative
//!   name, `kiln://<organization>/…`, whose path is the chain of whom it acts for, nearest last
//!   (as SPIFFE puts a workload's identity in one URI SAN):
//!
//!   | Actor | URI |
//!   |---|---|
//!   | a person | `kiln://<org>/person/<name>` |
//!   | an agent session at an epoch | `kiln://<org>/person/<name>/agent/<agent>/epoch/<n>` |
//!   | a runner process | `kiln://<org>/person/<enrolled by>/machine/<machine>/instance/<instance>` |
//!   | a CI leg | the runner's, then `/run/<run>/attempt/<n>/leg/<leg>` |
//!   | the server | `kiln://<org>/server` |
//!   | CD | `kiln://<org>/server/cd` |
//!
//! - **Facts that aren't names** ride in registered extensions only: a leg's workflow, repository,
//!   commit and run in Fulcio's (1.3.6.1.4.1.57264.1.*, DER UTF8Strings), so kiln's Sigstore
//!   verifier reads them too. kiln adds no OID of its own.
//! - **Checking** is RFC 5280's path validation at the time the caller gives (the time a
//!   signature was made, from the trail): every certificate signed by the next and the last by
//!   kiln's root, each valid then, the issuers CAs, the leaf not one and for digital signatures.
//! - **Revocations** are the caller's, each with the time it took effect: a certificate (by
//!   serial) or key (by its SPKI's SHA-256) revoked at T refuses what's checked at T or later,
//!   saying when, why and by whom; what was signed before T still verifies.
//! - **An agent is never its person.** [`Verified`] names the actor and, apart, whom it acts for;
//!   [`Identity::is_person`] is true only for a person's own certificate.
//!
//! No clock, disk or network: the time and the revocations are arguments.

use std::fmt;

use sha2::Digest as _;
use x509_parser::certificate::X509Certificate;
use x509_parser::extensions::GeneralName;

/// The scheme every kiln identity's URI has.
pub const SCHEME: &str = "kiln://";

/// Fulcio's extensions, under 1.3.6.1.4.1.57264.1: the arc and the members a leg carries
/// (Sigstore's OID directory, `docs/oid-info.md`).
pub mod fulcio {
    /// 1.3.6.1.4.1.57264.1
    pub const ARC: [u64; 8] = [1, 3, 6, 1, 4, 1, 57264, 1];
    /// Build Signer URI: the workflow that signs (its ref).
    pub const BUILD_SIGNER_URI: u64 = 9;
    /// Source Repository URI.
    pub const SOURCE_REPOSITORY_URI: u64 = 12;
    /// Source Repository Digest: the commit.
    pub const SOURCE_REPOSITORY_DIGEST: u64 = 13;
    /// Run Invocation URI: the run (and attempt).
    pub const RUN_INVOCATION_URI: u64 = 21;

    /// The OID of a member, as a dotted string.
    pub fn oid(member: u64) -> String {
        let mut s: Vec<String> = ARC.iter().map(u64::to_string).collect();
        s.push(member.to_string());
        s.join(".")
    }
}

/// The kinds of actor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Person,
    Agent,
    Runner,
    Leg,
    Server,
    Cd,
}

/// An actor, and the chain of whom it acts for inside its organization.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Actor {
    Person { name: String },
    Agent { person: String, agent: String, epoch: u64 },
    Runner { enrolled_by: String, machine: String, instance: String },
    Leg { enrolled_by: String, machine: String, instance: String, run: u64, attempt: u32, leg: String },
    Server,
    Cd,
}

/// Who a certificate says its holder is: an actor of an organization.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Identity {
    pub org: String,
    pub actor: Actor,
}

impl Identity {
    pub fn new(org: impl Into<String>, actor: Actor) -> Self {
        Self { org: org.into(), actor }
    }

    pub fn kind(&self) -> Kind {
        match self.actor {
            Actor::Person { .. } => Kind::Person,
            Actor::Agent { .. } => Kind::Agent,
            Actor::Runner { .. } => Kind::Runner,
            Actor::Leg { .. } => Kind::Leg,
            Actor::Server => Kind::Server,
            Actor::Cd => Kind::Cd,
        }
    }

    /// Its URI, as a certificate carries it. Fails when a part isn't a safe path segment.
    pub fn uri(&self) -> Result<String, Refusal> {
        let path = match &self.actor {
            Actor::Person { name } => vec!["person".into(), name.clone()],
            Actor::Agent { person, agent, epoch } => vec!["person".into(), person.clone(), "agent".into(), agent.clone(), "epoch".into(), epoch.to_string()],
            Actor::Runner { enrolled_by, machine, instance } => {
                vec!["person".into(), enrolled_by.clone(), "machine".into(), machine.clone(), "instance".into(), instance.clone()]
            }
            Actor::Leg { enrolled_by, machine, instance, run, attempt, leg } => vec![
                "person".into(),
                enrolled_by.clone(),
                "machine".into(),
                machine.clone(),
                "instance".into(),
                instance.clone(),
                "run".into(),
                run.to_string(),
                "attempt".into(),
                attempt.to_string(),
                "leg".into(),
                leg.clone(),
            ],
            Actor::Server => vec!["server".into()],
            Actor::Cd => vec!["server".into(), "cd".into()],
        };
        let mut uri = format!("{SCHEME}{}", self.org);
        for part in std::iter::once(&self.org).chain(&path) {
            segment(part).map_err(|why| Refusal::BadIdentity { uri: uri.clone(), why })?;
        }
        for part in &path {
            uri.push('/');
            uri.push_str(part);
        }
        Ok(uri)
    }

    /// The identity a URI names.
    pub fn parse(uri: &str) -> Result<Self, Refusal> {
        let bad = |why: &str| Refusal::BadIdentity { uri: uri.to_owned(), why: why.to_owned() };
        let rest = uri.strip_prefix(SCHEME).ok_or_else(|| bad("it isn't a kiln:// URI"))?;
        let parts: Vec<&str> = rest.split('/').collect();
        for p in &parts {
            segment(p).map_err(|why| bad(&why))?;
        }
        // Digits only, no leading zero: one spelling for each identity.
        let number = |s: &str, what: &str| {
            s.parse::<u64>()
                .ok()
                .filter(|_| s.bytes().all(|b| b.is_ascii_digit()) && (s == "0" || !s.starts_with('0')))
                .ok_or_else(|| bad(&format!("its {what} isn't a number (digits, no leading zero)")))
        };
        let (org, path) = parts.split_first().ok_or_else(|| bad("it names no organization"))?;
        let s = |x: &&str| (*x).to_owned();
        let actor = match path {
            ["person", name] => Actor::Person { name: s(name) },
            ["person", person, "agent", agent, "epoch", epoch] => Actor::Agent { person: s(person), agent: s(agent), epoch: number(epoch, "epoch")? },
            ["person", by, "machine", machine, "instance", instance] => Actor::Runner { enrolled_by: s(by), machine: s(machine), instance: s(instance) },
            ["person", by, "machine", machine, "instance", instance, "run", run, "attempt", attempt, "leg", leg] => Actor::Leg {
                enrolled_by: s(by),
                machine: s(machine),
                instance: s(instance),
                run: number(run, "run")?,
                attempt: u32::try_from(number(attempt, "attempt")?).map_err(|_| bad("its attempt is too large"))?,
                leg: s(leg),
            },
            ["server"] => Actor::Server,
            ["server", "cd"] => Actor::Cd,
            ["person", ..] | ["server", ..] => return Err(bad("its chain of whom it acts for is incomplete or unknown")),
            _ => return Err(bad("it names no kind of actor kiln knows (person, agent, runner, leg, server, cd)")),
        };
        Ok(Self { org: s(org), actor })
    }

    /// Whom it acts for, nearest first; the organization ([`Identity::org`]) is behind them all.
    pub fn acts_for(&self) -> Vec<Identity> {
        let id = |actor| Identity { org: self.org.clone(), actor };
        match &self.actor {
            Actor::Person { .. } | Actor::Server => Vec::new(),
            Actor::Agent { person, .. } => vec![id(Actor::Person { name: person.clone() })],
            Actor::Runner { enrolled_by, .. } => vec![id(Actor::Person { name: enrolled_by.clone() })],
            Actor::Leg { enrolled_by, machine, instance, .. } => vec![
                id(Actor::Runner { enrolled_by: enrolled_by.clone(), machine: machine.clone(), instance: instance.clone() }),
                id(Actor::Person { name: enrolled_by.clone() }),
            ],
            Actor::Cd => vec![id(Actor::Server)],
        }
    }

    /// Whether this is the person `name` themself (an agent acting for them never is).
    pub fn is_person(&self, name: &str) -> bool {
        matches!(&self.actor, Actor::Person { name: n } if n == name)
    }
}

impl fmt::Display for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let org = &self.org;
        match &self.actor {
            Actor::Person { name } => write!(f, "{name} of {org}"),
            Actor::Agent { person, agent, epoch } => write!(f, "agent {agent}, epoch {epoch}, for {person}, of {org}"),
            Actor::Runner { enrolled_by, machine, instance } => write!(f, "runner {machine} (instance {instance}), enrolled by {enrolled_by}, of {org}"),
            Actor::Leg { enrolled_by, machine, instance, run, attempt, leg } => {
                write!(f, "leg {leg} of run {run} (attempt {attempt}), on runner {machine} (instance {instance}), enrolled by {enrolled_by}, of {org}")
            }
            Actor::Server => write!(f, "kiln server of {org}"),
            Actor::Cd => write!(f, "CD, for the kiln server of {org}"),
        }
    }
}

/// A path segment of an identity: 1 to 128 of `A-Z a-z 0-9 . _ -`, never `.` or `..`.
fn segment(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("a part of it is empty".into());
    }
    if s.len() > 128 {
        return Err("a part of it is longer than 128 characters".into());
    }
    if s == "." || s == ".." {
        return Err("a part of it is . or ..".into());
    }
    if let Some(c) = s.chars().find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))) {
        return Err(format!("{c:?} isn't allowed in it (letters, digits, '.', '_' and '-' are)"));
    }
    Ok(())
}

/// A leg's facts from Fulcio's extensions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    /// Build Signer URI: the workflow, by ref.
    pub workflow: Option<String>,
    /// Source Repository URI.
    pub repository: Option<String>,
    /// Source Repository Digest: the commit.
    pub commit: Option<String>,
    /// Run Invocation URI.
    pub run: Option<String>,
}

/// A revocation kiln recorded: a certificate (its serial, hex) or a key (its SPKI's SHA-256, hex),
/// from `at` (unix seconds) on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Revocation {
    pub serial: Option<String>,
    pub key: Option<String>,
    pub at: i64,
    pub reason: String,
    pub by: String,
}

impl Revocation {
    fn covers(&self, serial: &str, key: &str) -> bool {
        self.serial.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(serial)) || self.key.as_deref().is_some_and(|k| k.eq_ignore_ascii_case(key))
    }
}

/// A certificate whose chain held: who it names, and what it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    pub identity: Identity,
    /// The leaf's serial number, hex.
    pub serial: String,
    /// The SHA-256 of the leaf's public key (its SubjectPublicKeyInfo), hex.
    pub key: String,
    pub not_before: i64,
    pub not_after: i64,
    pub facts: Facts,
}

impl Verified {
    /// Whom it acts for, nearest first (the organization is [`Identity::org`]).
    pub fn acts_for(&self) -> Vec<Identity> {
        self.identity.acts_for()
    }
}

impl fmt::Display for Verified {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.identity.actor, &self.facts.commit) {
            (Actor::Leg { enrolled_by, machine, instance, run, attempt, leg }, Some(commit)) => {
                let short = commit.get(..7).unwrap_or(commit);
                write!(
                    f,
                    "leg {leg} of run {run} (attempt {attempt}) at {short}, on runner {machine} (instance {instance}), enrolled by {enrolled_by}, of {}",
                    self.identity.org
                )
            }
            _ => self.identity.fmt(f),
        }
    }
}

/// Why a certificate, or a signature under it, isn't taken.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("no certificate to check")]
    Empty,
    #[error("not a certificate: {0}")]
    Malformed(String),
    #[error("kiln's root isn't a usable CA certificate: {0}")]
    BadRoot(String),
    #[error("{subject} wasn't issued by {issuer}")]
    NotIssued { subject: String, issuer: String },
    #[error("{subject} isn't a CA, so it can't issue certificates")]
    NotACa { subject: String },
    #[error("{subject} expired at {not_after} (checked at {at})")]
    Expired { subject: String, not_after: String, at: String },
    #[error("{subject} isn't valid until {not_before} (checked at {at})")]
    NotYetValid { subject: String, not_before: String, at: String },
    #[error("{subject} was revoked at {revoked_at} ({reason}, by {by}); checked at {at}")]
    Revoked { subject: String, revoked_at: String, reason: String, by: String, at: String },
    #[error("{subject} isn't a leaf for signing: {why}")]
    NotALeaf { subject: String, why: String },
    #[error("the certificate names no kiln identity (a kiln:// URI subject alternative name)")]
    NoIdentity,
    #[error("the certificate names {0} URIs; an identity is exactly one")]
    ManyIdentities(usize),
    #[error("{uri:?} isn't a kiln identity: {why}")]
    BadIdentity { uri: String, why: String },
    #[error("{who}'s certificate is incomplete: {why}")]
    Incomplete { who: String, why: String },
    #[error("{who}'s key is of a kind kiln doesn't verify ({alg})")]
    UnsupportedKey { who: String, alg: String },
    #[error("the signature isn't {who}'s")]
    BadSignature { who: String },
}

/// A unix time, as RFC 3339 (the number itself when it's out of range).
fn when(secs: i64) -> String {
    jiff::Timestamp::from_second(secs).map(|t| t.to_string()).unwrap_or_else(|_| secs.to_string())
}

fn parse(der: &[u8]) -> Result<X509Certificate<'_>, Refusal> {
    let (rest, cert) = x509_parser::parse_x509_certificate(der).map_err(|e| Refusal::Malformed(e.to_string()))?;
    if !rest.is_empty() {
        return Err(Refusal::Malformed(format!("{} bytes after the certificate", rest.len())));
    }
    Ok(cert)
}

/// The URIs among a certificate's subject alternative names.
fn uris<'a>(cert: &'a X509Certificate<'_>) -> Result<Vec<&'a str>, Refusal> {
    let san = cert.subject_alternative_name().map_err(|e| Refusal::Malformed(format!("its subject alternative names: {e}")))?;
    Ok(san.map(|s| s.value.general_names.iter().filter_map(|n| if let GeneralName::URI(u) = n { Some(*u) } else { None }).collect()).unwrap_or_default())
}

/// How a certificate is named in a refusal: its kiln URI, else its subject.
fn name_of(cert: &X509Certificate<'_>) -> String {
    match uris(cert).ok().and_then(|u| u.into_iter().find(|u| u.starts_with(SCHEME))) {
        Some(u) => u.to_owned(),
        None => cert.subject().to_string(),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A DER UTF8String's text (Fulcio's later extensions are), or `None`.
fn der_utf8(der: &[u8]) -> Option<String> {
    let (&tag, rest) = der.split_first()?;
    if tag != 0x0c {
        return None;
    }
    let (&first, rest) = rest.split_first()?;
    let (len, body) = match first {
        n @ 0..=0x7f => (usize::from(n), rest),
        0x81 => {
            let (&n, rest) = rest.split_first()?;
            (usize::from(n), rest)
        }
        0x82 => {
            let (hi, rest) = rest.split_first()?;
            let (lo, rest) = rest.split_first()?;
            (usize::from(*hi) << 8 | usize::from(*lo), rest)
        }
        _ => return None,
    };
    if body.len() != len {
        return None;
    }
    String::from_utf8(body.to_vec()).ok()
}

/// `s` as a DER UTF8String, for an issuer to put in Fulcio's extensions (at most 65 535 bytes).
pub fn der_utf8_string(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    let mut out = vec![0x0c];
    match u16::try_from(b.len()).ok()? {
        n @ 0..=0x7f => out.push(u8::try_from(n).ok()?),
        n @ 0x80..=0xff => out.extend([0x81, u8::try_from(n).ok()?]),
        n => out.extend([0x82, u8::try_from(n >> 8).ok()?, u8::try_from(n & 0xff).ok()?]),
    }
    out.extend_from_slice(b);
    Some(out)
}

fn facts(cert: &X509Certificate<'_>) -> Facts {
    let mut f = Facts::default();
    for ext in cert.extensions() {
        let oid = ext.oid.to_id_string();
        let slot = match oid {
            o if o == fulcio::oid(fulcio::BUILD_SIGNER_URI) => &mut f.workflow,
            o if o == fulcio::oid(fulcio::SOURCE_REPOSITORY_URI) => &mut f.repository,
            o if o == fulcio::oid(fulcio::SOURCE_REPOSITORY_DIGEST) => &mut f.commit,
            o if o == fulcio::oid(fulcio::RUN_INVOCATION_URI) => &mut f.run,
            _ => continue,
        };
        *slot = der_utf8(ext.value);
    }
    f
}

/// Valid at `at`, and not revoked by then.
fn current(cert: &X509Certificate<'_>, revocations: &[Revocation], at: i64) -> Result<(), Refusal> {
    let subject = name_of(cert);
    let (nb, na) = (cert.validity().not_before.timestamp(), cert.validity().not_after.timestamp());
    if at < nb {
        return Err(Refusal::NotYetValid { subject, not_before: when(nb), at: when(at) });
    }
    if at > na {
        return Err(Refusal::Expired { subject, not_after: when(na), at: when(at) });
    }
    let (serial, key) = (hex(cert.raw_serial()), hex(&sha2::Sha256::digest(cert.public_key().raw)));
    if let Some(r) = revocations.iter().filter(|r| r.at <= at && r.covers(&serial, &key)).min_by_key(|r| r.at) {
        return Err(Refusal::Revoked { subject, revoked_at: when(r.at), reason: r.reason.clone(), by: r.by.clone(), at: when(at) });
    }
    Ok(())
}

/// May issue certificates: a CA, and keyCertSign when it lists key usages.
fn issuer_ok(cert: &X509Certificate<'_>) -> bool {
    let ku = cert.key_usage().ok().flatten().map(|k| k.value.key_cert_sign());
    cert.is_ca() && ku.unwrap_or(true)
}

/// Checks `chain` (the leaf first, then any intermediates, each issued by the next) to kiln's
/// root `root` at `at` (unix seconds: when what it signed was signed), against `revocations`.
pub fn verify(chain: &[&[u8]], root: &[u8], revocations: &[Revocation], at: i64) -> Result<Verified, Refusal> {
    verified(chain, root, revocations, at).map(|(v, _, _)| v)
}

/// [`verify`], then whether `signature` over `message` is the leaf's key's: who signed it, or why
/// it isn't taken. ECDSA P-256 with SHA-256, P-384 with SHA-384 (ASN.1 signatures), and Ed25519.
pub fn verify_signed(chain: &[&[u8]], root: &[u8], revocations: &[Revocation], at: i64, message: &[u8], signature: &[u8]) -> Result<Verified, Refusal> {
    use aws_lc_rs::signature::{ECDSA_P256_SHA256_ASN1, ECDSA_P384_SHA384_ASN1, ED25519, UnparsedPublicKey, VerificationAlgorithm};
    let (v, alg, key) = verified(chain, root, revocations, at)?;
    let who = v.to_string();
    let algorithm: &'static dyn VerificationAlgorithm = match alg.as_str() {
        "1.2.840.10045.3.1.7" => &ECDSA_P256_SHA256_ASN1,
        "1.3.132.0.34" => &ECDSA_P384_SHA384_ASN1,
        "1.3.101.112" => &ED25519,
        other => return Err(Refusal::UnsupportedKey { who, alg: other.to_owned() }),
    };
    UnparsedPublicKey::new(algorithm, &key).verify(message, signature).map_err(|_| Refusal::BadSignature { who })?;
    Ok(v)
}

/// The chain checked, with the leaf's key algorithm (its curve's OID for EC keys) and key bytes.
fn verified(chain: &[&[u8]], root: &[u8], revocations: &[Revocation], at: i64) -> Result<(Verified, String, Vec<u8>), Refusal> {
    let root = parse(root).map_err(|e| Refusal::BadRoot(e.to_string()))?;
    if !issuer_ok(&root) {
        return Err(Refusal::BadRoot("it isn't a CA".into()));
    }
    root.verify_signature(None).map_err(|_| Refusal::BadRoot("it isn't self-signed".into()))?;
    current(&root, revocations, at)?;
    let certs = chain.iter().map(|d| parse(d)).collect::<Result<Vec<_>, _>>()?;
    let leaf = certs.first().ok_or(Refusal::Empty)?;
    for (cert, issuer) in certs.iter().zip(certs.iter().skip(1).chain(std::iter::once(&root))) {
        if !issuer_ok(issuer) {
            return Err(Refusal::NotACa { subject: name_of(issuer) });
        }
        cert.verify_signature(Some(issuer.public_key())).map_err(|_| Refusal::NotIssued { subject: name_of(cert), issuer: name_of(issuer) })?;
        current(cert, revocations, at)?;
    }
    let subject = name_of(leaf);
    if leaf.is_ca() {
        return Err(Refusal::NotALeaf { subject, why: "it is a CA".into() });
    }
    match leaf.key_usage() {
        Ok(Some(ku)) if ku.value.digital_signature() && !ku.value.key_cert_sign() && !ku.value.crl_sign() => {}
        Ok(Some(_)) => return Err(Refusal::NotALeaf { subject, why: "its key usages aren't digitalSignature alone".into() }),
        Ok(None) => return Err(Refusal::NotALeaf { subject, why: "it lists no key usages".into() }),
        Err(e) => return Err(Refusal::Malformed(format!("its key usages: {e}"))),
    }
    let all = uris(leaf)?;
    let identity = match all.as_slice() {
        [] => return Err(Refusal::NoIdentity),
        [one] => Identity::parse(one)?,
        many => return Err(Refusal::ManyIdentities(many.len())),
    };
    let facts = facts(leaf);
    if identity.kind() == Kind::Leg && facts.commit.is_none() {
        return Err(Refusal::Incomplete { who: identity.to_string(), why: "a leg's certificate names no commit (Fulcio's Source Repository Digest)".into() });
    }
    let spki = leaf.public_key();
    let alg = match spki.algorithm.algorithm.to_id_string().as_str() {
        "1.2.840.10045.2.1" => spki.algorithm.parameters.as_ref().and_then(|p| p.as_oid().ok()).map(|o| o.to_id_string()).unwrap_or_default(),
        other => other.to_owned(),
    };
    let v = Verified {
        identity,
        serial: hex(leaf.raw_serial()),
        key: hex(&sha2::Sha256::digest(spki.raw)),
        not_before: leaf.validity().not_before.timestamp(),
        not_after: leaf.validity().not_after.timestamp(),
        facts,
    };
    Ok((v, alg, spki.subject_public_key.data.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{BasicConstraints, CertificateParams, CustomExtension, IsCa, Issuer, KeyPair, KeyUsagePurpose, SanType, SerialNumber, SigningKey as _, date_time_ymd};

    /// 2026-09-21: inside every test leaf's validity (2026-01-01 to 2027-01-01).
    const AT: i64 = 1_790_000_000;

    struct Ca {
        der: Vec<u8>,
        issuer: Issuer<'static, KeyPair>,
    }

    fn ca(name: &str) -> anyhow::Result<Ca> {
        let key = KeyPair::generate()?;
        let mut p = CertificateParams::new(Vec::<String>::new())?;
        p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        p.distinguished_name.push(rcgen::DnType::CommonName, name);
        p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
        p.not_before = date_time_ymd(2025, 1, 1);
        p.not_after = date_time_ymd(2036, 1, 1);
        let cert = p.self_signed(&key)?;
        Ok(Ca { der: cert.der().to_vec(), issuer: Issuer::new(p, key) })
    }

    struct Leaf {
        der: Vec<u8>,
        key: KeyPair,
    }

    fn params(uris: &[&str], serial: u8) -> anyhow::Result<CertificateParams> {
        let mut p = CertificateParams::new(Vec::<String>::new())?;
        p.subject_alt_names = uris.iter().map(|u| Ok(SanType::URI((*u).try_into()?))).collect::<anyhow::Result<_>>()?;
        p.is_ca = IsCa::ExplicitNoCa;
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        p.not_before = date_time_ymd(2026, 1, 1);
        p.not_after = date_time_ymd(2027, 1, 1);
        p.serial_number = Some(SerialNumber::from_slice(&[0x40, serial]));
        Ok(p)
    }

    fn issue(ca: &Ca, p: CertificateParams) -> anyhow::Result<Leaf> {
        let key = KeyPair::generate()?;
        let der = p.signed_by(&key, &ca.issuer)?.der().to_vec();
        Ok(Leaf { der, key })
    }

    fn leaf(ca: &Ca, uri: &str) -> anyhow::Result<Leaf> {
        issue(ca, params(&[uri], 1)?)
    }

    fn fulcio_ext(member: u64, value: &str) -> anyhow::Result<CustomExtension> {
        let mut oid = fulcio::ARC.to_vec();
        oid.push(member);
        Ok(CustomExtension::from_oid_content(&oid, der_utf8_string(value).ok_or_else(|| anyhow::anyhow!("too long"))?))
    }

    fn leg(ca: &Ca, commit: Option<&str>) -> anyhow::Result<Leaf> {
        let mut p = params(&["kiln://hyper-light/person/ada/machine/fedora-2/instance/01a174ed/run/812/attempt/1/leg/e2e"], 7)?;
        p.custom_extensions.push(fulcio_ext(fulcio::BUILD_SIGNER_URI, "hyper-light/kiln/.kiln/ci.yml@refs/heads/main")?);
        p.custom_extensions.push(fulcio_ext(fulcio::SOURCE_REPOSITORY_URI, "https://github.com/hyper-light/kiln")?);
        p.custom_extensions.push(fulcio_ext(fulcio::RUN_INVOCATION_URI, "https://kiln.home/ci/runs/812/attempts/1")?);
        if let Some(c) = commit {
            p.custom_extensions.push(fulcio_ext(fulcio::SOURCE_REPOSITORY_DIGEST, c)?);
        }
        issue(ca, p)
    }

    fn check(ca: &Ca, l: &Leaf) -> Result<Verified, Refusal> {
        verify(&[&l.der], &ca.der, &[], AT)
    }

    #[test]
    fn w27_r01_chain_person() -> anyhow::Result<()> {
        let ca = ca("kiln CA")?;
        let v = check(&ca, &leaf(&ca, "kiln://hyper-light/person/sam")?)?;
        assert_eq!(v.identity, Identity::new("hyper-light", Actor::Person { name: "sam".into() }));
        assert_eq!(v.to_string(), "sam of hyper-light");
        assert!(v.acts_for().is_empty());
        assert!(v.identity.is_person("sam"));
        Ok(())
    }

    #[test]
    fn w27_r01_chain_agent_never_its_person() -> anyhow::Result<()> {
        let ca = ca("kiln CA")?;
        let v = check(&ca, &leaf(&ca, "kiln://hyper-light/person/sam/agent/matrix/epoch/3")?)?;
        assert_eq!(v.to_string(), "agent matrix, epoch 3, for sam, of hyper-light");
        assert_eq!(v.identity.actor, Actor::Agent { person: "sam".into(), agent: "matrix".into(), epoch: 3 });
        assert_eq!(v.acts_for(), vec![Identity::new("hyper-light", Actor::Person { name: "sam".into() })]);
        assert!(!v.identity.is_person("sam"), "an agent verifies as the agent, never as its person");
        assert_eq!(v.identity.kind(), Kind::Agent);
        Ok(())
    }

    #[test]
    fn w27_r01_chain_runner() -> anyhow::Result<()> {
        let ca = ca("kiln CA")?;
        let v = check(&ca, &leaf(&ca, "kiln://hyper-light/person/ada/machine/fedora-2/instance/01a174ed")?)?;
        assert_eq!(v.to_string(), "runner fedora-2 (instance 01a174ed), enrolled by ada, of hyper-light");
        assert_eq!(v.acts_for(), vec![Identity::new("hyper-light", Actor::Person { name: "ada".into() })]);
        Ok(())
    }

    #[test]
    fn w27_r01_chain_leg() -> anyhow::Result<()> {
        let ca = ca("kiln CA")?;
        let v = check(&ca, &leg(&ca, Some("4c1e2a90d1f3b5a7c9e1f3a5b7c9d1e3f5a7b9c1"))?)?;
        assert_eq!(v.to_string(), "leg e2e of run 812 (attempt 1) at 4c1e2a9, on runner fedora-2 (instance 01a174ed), enrolled by ada, of hyper-light");
        assert_eq!(v.facts.commit.as_deref(), Some("4c1e2a90d1f3b5a7c9e1f3a5b7c9d1e3f5a7b9c1"));
        assert_eq!(v.facts.workflow.as_deref(), Some("hyper-light/kiln/.kiln/ci.yml@refs/heads/main"));
        assert_eq!(v.facts.repository.as_deref(), Some("https://github.com/hyper-light/kiln"));
        assert_eq!(v.facts.run.as_deref(), Some("https://kiln.home/ci/runs/812/attempts/1"));
        let kinds: Vec<Kind> = v.acts_for().iter().map(Identity::kind).collect();
        assert_eq!(kinds, vec![Kind::Runner, Kind::Person]);
        Ok(())
    }

    #[test]
    fn w27_r01_chain_server_and_cd() -> anyhow::Result<()> {
        let ca = ca("kiln CA")?;
        let server = check(&ca, &leaf(&ca, "kiln://hyper-light/server")?)?;
        assert_eq!(server.to_string(), "kiln server of hyper-light");
        let cd = check(&ca, &leaf(&ca, "kiln://hyper-light/server/cd")?)?;
        assert_eq!(cd.to_string(), "CD, for the kiln server of hyper-light");
        assert_eq!(cd.acts_for(), vec![server.identity]);
        Ok(())
    }

    #[test]
    fn w27_r01_uri_round_trips() -> anyhow::Result<()> {
        for uri in [
            "kiln://hyper-light/person/sam",
            "kiln://hyper-light/person/sam/agent/matrix/epoch/3",
            "kiln://hyper-light/person/ada/machine/fedora-2/instance/01a174ed",
            "kiln://hyper-light/person/ada/machine/fedora-2/instance/01a174ed/run/812/attempt/1/leg/e2e",
            "kiln://hyper-light/server",
            "kiln://hyper-light/server/cd",
        ] {
            assert_eq!(Identity::parse(uri)?.uri()?, uri);
        }
        Ok(())
    }

    #[test]
    fn w27_r01_bad_identities_refused() {
        for (uri, says) in [
            ("https://hyper-light/person/sam", "isn't a kiln:// URI"),
            ("kiln://hyper-light/agent/matrix/epoch/3", "no kind of actor"),
            ("kiln://hyper-light/person/sam/agent/matrix", "incomplete"),
            ("kiln://hyper-light/person/sam/agent/matrix/epoch/three", "isn't a number"),
            ("kiln://hyper-light/person/sam/agent/matrix/epoch/+3", "isn't allowed"),
            ("kiln://hyper-light/person/sam/agent/matrix/epoch/03", "isn't a number"),
            ("kiln://hyper-light/person/../agent/matrix/epoch/3", ". or .."),
            ("kiln://hyper-light/person/s%61m", "isn't allowed"),
            ("kiln:///person/sam", "empty"),
        ] {
            let got = Identity::parse(uri).map(|i| i.to_string());
            assert!(matches!(&got, Err(e) if e.to_string().contains(says)), "{uri}: {got:?}");
        }
        assert!(Identity::new("hyper light", Actor::Server).uri().is_err());
    }

    #[test]
    fn w27_r01_expired_and_not_yet_valid() -> anyhow::Result<()> {
        let ca = ca("kiln CA")?;
        let l = leaf(&ca, "kiln://hyper-light/person/sam")?;
        // 2027-12-15: after the leaf's end.
        let late = verify(&[&l.der], &ca.der, &[], 1_830_000_000);
        assert!(matches!(&late, Err(Refusal::Expired { subject, not_after, .. }) if subject == "kiln://hyper-light/person/sam" && not_after.starts_with("2027-01-01")), "{late:?}");
        // 2025-10-09: before it began (the root is valid then).
        let early = verify(&[&l.der], &ca.der, &[], 1_760_000_000);
        assert!(matches!(&early, Err(Refusal::NotYetValid { not_before, .. }) if not_before.starts_with("2026-01-01")), "{early:?}");
        Ok(())
    }

    #[test]
    fn w27_r01_revoked_from_the_moment_of_revocation() -> anyhow::Result<()> {
        let ca = ca("kiln CA")?;
        let l = leaf(&ca, "kiln://hyper-light/person/ada/machine/fedora-2/instance/01a174ed")?;
        let v = check(&ca, &l)?;
        let by_serial = Revocation { serial: Some(v.serial.clone()), key: None, at: AT, reason: "runner retired".into(), by: "ada".into() };
        // Signed a second before: still verifies.
        assert!(verify(&[&l.der], &ca.der, std::slice::from_ref(&by_serial), AT - 1).is_ok());
        // Signed at the moment, or after: refused, saying when, why and by whom.
        for at in [AT, AT + 3600] {
            let got = verify(&[&l.der], &ca.der, std::slice::from_ref(&by_serial), at);
            assert!(
                matches!(&got, Err(e @ Refusal::Revoked { .. }) if e.to_string().contains("revoked at 2026-09-21") && e.to_string().contains("runner retired, by ada")),
                "{got:?}"
            );
        }
        // A key revoked (lost) refuses every certificate for it.
        let by_key = Revocation { serial: None, key: Some(v.key.to_uppercase()), at: AT, reason: "key lost".into(), by: "ada".into() };
        assert!(matches!(verify(&[&l.der], &ca.der, &[by_key], AT), Err(Refusal::Revoked { reason, .. }) if reason == "key lost"));
        // Another certificate's revocation doesn't touch it.
        let other = Revocation { serial: Some("4002".into()), key: None, at: 0, reason: "other".into(), by: "ada".into() };
        assert!(verify(&[&l.der], &ca.der, &[other], AT).is_ok());
        Ok(())
    }

    #[test]
    fn w27_r01_wrongly_issued_refused() -> anyhow::Result<()> {
        let kiln = ca("kiln CA")?;
        let other = ca("someone else's CA")?;
        // Issued by another CA.
        let foreign = leaf(&other, "kiln://hyper-light/person/sam")?;
        assert!(matches!(check(&kiln, &foreign), Err(Refusal::NotIssued { .. })));
        // Self-signed.
        let key = KeyPair::generate()?;
        let selfsigned = params(&["kiln://hyper-light/person/sam"], 2)?.self_signed(&key)?.der().to_vec();
        assert!(matches!(verify(&[&selfsigned], &kiln.der, &[], AT), Err(Refusal::NotIssued { .. })));
        // A leaf issuing a leaf.
        let mid = leaf(&kiln, "kiln://hyper-light/person/sam")?;
        let mid_issuer = Issuer::new(params(&["kiln://hyper-light/person/sam"], 1)?, mid.key);
        let k2 = KeyPair::generate()?;
        let grandchild = params(&["kiln://hyper-light/person/sam/agent/matrix/epoch/1"], 3)?.signed_by(&k2, &mid_issuer)?.der().to_vec();
        assert!(matches!(verify(&[&grandchild, &mid.der], &kiln.der, &[], AT), Err(Refusal::NotACa { .. })));
        // Two URIs.
        let two = issue(&kiln, params(&["kiln://hyper-light/person/sam", "kiln://hyper-light/server"], 4)?)?;
        assert!(matches!(check(&kiln, &two), Err(Refusal::ManyIdentities(2))));
        // None.
        let none = issue(&kiln, params(&[], 5)?)?;
        assert!(matches!(check(&kiln, &none), Err(Refusal::NoIdentity)));
        // A CA as the leaf.
        let mut p = params(&["kiln://hyper-light/server"], 6)?;
        p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::DigitalSignature];
        assert!(matches!(check(&kiln, &issue(&kiln, p)?), Err(Refusal::NotALeaf { .. })));
        // An agent's path without its person.
        assert!(matches!(check(&kiln, &leaf(&kiln, "kiln://hyper-light/agent/matrix/epoch/3")?), Err(Refusal::BadIdentity { .. })));
        // A leg that names no commit.
        assert!(matches!(check(&kiln, &leg(&kiln, None)?), Err(Refusal::Incomplete { .. })));
        // Not kiln's root: a leaf passed as the root.
        let l = leaf(&kiln, "kiln://hyper-light/person/sam")?;
        assert!(matches!(verify(&[&l.der], &l.der, &[], AT), Err(Refusal::BadRoot(_))));
        assert!(matches!(verify(&[], &kiln.der, &[], AT), Err(Refusal::Empty)));
        assert!(matches!(verify(&[b"not der"], &kiln.der, &[], AT), Err(Refusal::Malformed(_))));
        Ok(())
    }

    #[test]
    fn w27_r01_signature_verifies_as_its_signer() -> anyhow::Result<()> {
        let ca = ca("kiln CA")?;
        let agent = leaf(&ca, "kiln://hyper-light/person/sam/agent/matrix/epoch/3")?;
        let msg = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n";
        let sig = agent.key.sign(msg)?;
        let v = verify_signed(&[&agent.der], &ca.der, &[], AT, msg, &sig)?;
        assert_eq!(v.to_string(), "agent matrix, epoch 3, for sam, of hyper-light");
        assert!(!v.identity.is_person("sam"));
        // Another message, or another key's signature: refused, naming whose it isn't.
        let got = verify_signed(&[&agent.der], &ca.der, &[], AT, b"other", &sig);
        assert!(matches!(&got, Err(Refusal::BadSignature { who }) if who == "agent matrix, epoch 3, for sam, of hyper-light"), "{got:?}");
        let sam = leaf(&ca, "kiln://hyper-light/person/sam")?;
        assert!(matches!(verify_signed(&[&sam.der], &ca.der, &[], AT, msg, &sig), Err(Refusal::BadSignature { .. })));
        Ok(())
    }

    #[test]
    fn w27_r01_der_utf8_round_trips() {
        for s in ["", "a", &"x".repeat(127), &"y".repeat(128), &"z".repeat(300)] {
            assert_eq!(der_utf8_string(s).and_then(|d| der_utf8(&d)).as_deref(), Some(s));
        }
        assert_eq!(der_utf8(&[0x0c, 0x05, b'a']), None);
        assert_eq!(der_utf8(&[0x13, 0x01, b'a']), None);
    }
}
