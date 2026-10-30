//! One signed statement (docs/ci.md W27-R03 to R08's base), checked purely: what an actor signs
//! (a custody record, a delegation, and later a commit's, a verdict's, a leg's or a land's words),
//! how a statement is named, and whether kiln takes it, as of a time kiln vouches for.
//!
//! - **A statement** is a DSSE envelope (its payload type, its payload, one signature over DSSE's
//!   PAE, as W24's seal signs) in a Sigstore bundle with its signer's certificate, the shape kiln's
//!   attestations already take ([`crate::attest`]). Its payload, a [`Statement`], says, signed:
//!   what it's about, by digest (in-toto's subjects); the earlier statements it follows, by
//!   theirs, so records chain (a receipt follows its hand-on, the next hop's hand-on that receipt);
//!   its number on one of its signer's streams, so a record dropped shows as a gap; the delegation
//!   its signer acts under, if any; and its payload type's own words, its body.
//! - **A statement's digest** ([`digest`]) is SHA-256 over its PAE: what refers to it, the same
//!   however often and by whatever it's signed.
//! - **Checking** ([`verify`]) is its signer's chain and signature ([`chain::verify_signed`]) at the
//!   time kiln vouches for, its own record of when it took the statement, never a time the signer
//!   claims (certificates are short-lived, or revoked as they're left behind); then its payload's
//!   form. It says who signed and whom they act for, or why it isn't taken.
//! - **Under** ([`verify_under`]): a statement whose signer acts under a delegation is checked
//!   through kiln's records, link by link: each a delegation ([`PAYLOAD_DELEGATION`]) that verifies
//!   at the time kiln took it, taken no later than what acts under it, and naming as its `delegate`
//!   the signer below it; the last acts under none. What a delegation entitles its delegate to is
//!   its body's, judged over the links this returns (the Transfers plan's).
//! - **Sequences** ([`gaps`]): a custodian (a statement's signer) numbers its statements on each of
//!   its streams from 1; a number none has, or two statements have, is named.
//!
//! No clock, disk or network: the times, the revocations and kiln's records are arguments.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use super::chain::{self, Revocation, Verified};
use crate::attest::{self, Subject};

/// A custody record's payload type: a thing received, handed on, passed on or taken, by its
/// digest (the Transfers plan's body).
pub const PAYLOAD_CUSTODY: &str = "application/vnd.kiln.custody+json";
/// A delegation's payload type: its body names its `delegate` (an identity's URI) and what it
/// entitles them to (the Transfers plan's).
pub const PAYLOAD_DELEGATION: &str = "application/vnd.kiln.delegation+json";
/// The version of [`Statement`] kiln writes and reads.
pub const VERSION: u32 = 1;
/// The most delegations a statement acts under, one under another.
pub const MAX_UNDER: usize = 16;

/// What a statement says, signed: its payload, as JSON.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Statement {
    /// [`VERSION`].
    pub v: u32,
    /// What it's about, each by its digests (in-toto's subjects).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subject: Vec<Subject>,
    /// The earlier statements it follows, by their digests ([`digest`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub follows: Vec<String>,
    /// The stream of its signer's it's numbered on, with [`Statement::seq`]: both or neither.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<String>,
    /// Its number on that stream, from 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// The delegation its signer acts under, by its digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub under: Option<String>,
    /// What its payload type says.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub body: Value,
}

impl Statement {
    /// A statement of `body`, about nothing yet, following nothing.
    pub fn new(body: Value) -> Self {
        Self { v: VERSION, body, ..Self::default() }
    }

    /// Whether it's well formed, or why not.
    fn check(&self) -> Result<(), String> {
        if self.v != VERSION {
            return Err(format!("it's version {} of kiln's statements; this kiln reads version {VERSION}", self.v));
        }
        for s in &self.subject {
            if s.digest.is_empty() {
                return Err(format!("its subject {:?} has no digest", s.name));
            }
            if let Some((alg, _)) = s.digest.iter().find(|(alg, hex)| !is_hex(hex) || (alg.as_str() == "sha256" && hex.len() != 64)) {
                return Err(format!("its subject {:?} has a malformed {alg} digest", s.name));
            }
        }
        if let Some(d) = self.follows.iter().chain(&self.under).find(|d| !is_digest(d)) {
            return Err(format!("{d:?} isn't a statement's digest (sha256: and 64 lowercase hex digits)"));
        }
        let mut seen = BTreeSet::new();
        if let Some(d) = self.follows.iter().find(|d| !seen.insert(*d)) {
            return Err(format!("it follows {d} twice"));
        }
        match (&self.stream, self.seq) {
            (None, None) => Ok(()),
            (Some(s), Some(_)) if s.is_empty() || s.len() > 256 || s.chars().any(char::is_control) => {
                Err(format!("its stream {s:?} isn't a stream's name (1 to 256 bytes, no control characters)"))
            }
            (Some(_), Some(0)) => Err("its number is 0; a stream's are numbered from 1".into()),
            (Some(_), Some(_)) => Ok(()),
            (Some(s), None) => Err(format!("it names stream {s:?} but no number on it")),
            (None, Some(n)) => Err(format!("it's numbered {n} but names no stream")),
        }
    }
}

fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_digest(s: &str) -> bool {
    s.strip_prefix("sha256:").is_some_and(|h| h.len() == 64 && is_hex(h))
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// A statement's digest: `sha256:` and the SHA-256 of its PAE, in hex. It doesn't depend on the
/// statement's signatures: signed again, or by another key, it's the same statement.
pub fn digest(payload_type: &str, payload: &[u8]) -> String {
    format!("sha256:{}", chain::hex(&Sha256::digest(attest::pae(payload_type, payload))))
}

/// A statement's payload as it's signed (its JSON), once it's well formed.
pub fn payload(statement: &Statement) -> Result<Vec<u8>, Refusal> {
    statement.check().map_err(Refusal::Malformed)?;
    serde_json::to_vec(statement).map_err(|e| Refusal::Malformed(e.to_string()))
}

/// What a signer signs for `payload` of `payload_type`: DSSE's PAE.
pub fn to_sign(payload_type: &str, payload: &[u8]) -> Vec<u8> {
    attest::pae(payload_type, payload)
}

/// A signed statement: `payload` of `payload_type`, with `signature` over [`to_sign`]'s bytes by
/// the key `certificate` (DER) certifies, in a Sigstore bundle.
pub fn bundle(payload_type: &str, payload: &[u8], signature: &[u8], certificate: &[u8]) -> Value {
    json!({
        "mediaType": attest::BUNDLE,
        "verificationMaterial": { "certificate": { "rawBytes": b64(certificate) } },
        "dsseEnvelope": { "payloadType": payload_type, "payload": b64(payload), "signatures": [{ "sig": b64(signature) }] },
    })
}

/// A bundle's parts: its payload type and payload, its one signature, and its signer's chain
/// (the leaf first).
struct Opened {
    payload_type: String,
    payload: Vec<u8>,
    signature: Vec<u8>,
    chain: Vec<Vec<u8>>,
}

fn open(bundle: &Value) -> Result<Opened, Refusal> {
    let bad = |why: String| Refusal::Malformed(why);
    let dec = |what: &str, v: Option<&Value>| {
        let s = v.and_then(Value::as_str).ok_or_else(|| bad(format!("it has no {what}")))?;
        base64::engine::general_purpose::STANDARD.decode(s).map_err(|e| bad(format!("its {what} isn't base64: {e}")))
    };
    let media = bundle.get("mediaType").and_then(Value::as_str).unwrap_or_default();
    if !media.starts_with("application/vnd.dev.sigstore.bundle") {
        return Err(bad(format!("it isn't a Sigstore bundle (its mediaType is {media:?})")));
    }
    let material = bundle.get("verificationMaterial");
    let chain = match material.and_then(|m| m.pointer("/x509CertificateChain/certificates")).and_then(Value::as_array) {
        Some(certs) => certs.iter().map(|c| dec("certificate", c.get("rawBytes"))).collect::<Result<Vec<_>, _>>()?,
        None => vec![dec("certificate", material.and_then(|m| m.pointer("/certificate/rawBytes")))?],
    };
    let envelope = bundle.get("dsseEnvelope").ok_or_else(|| bad("it signs no DSSE envelope".into()))?;
    let payload_type = envelope.get("payloadType").and_then(Value::as_str).filter(|t| !t.is_empty()).ok_or_else(|| bad("its envelope has no payload type".into()))?;
    let payload = dec("payload", envelope.get("payload"))?;
    let signature = match envelope.get("signatures").and_then(Value::as_array).map(Vec::as_slice) {
        Some([one]) => dec("signature", one.get("sig"))?,
        Some(sigs) => return Err(bad(format!("its envelope has {} signatures; a statement has one", sigs.len()))),
        None => return Err(bad("its envelope has no signatures".into())),
    };
    Ok(Opened { payload_type: payload_type.to_owned(), payload, signature, chain })
}

/// A statement kiln takes: what it says, its digest, and who signed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checked {
    pub digest: String,
    pub payload_type: String,
    pub statement: Statement,
    /// Its signer, as its certificate says, checked at [`Checked::at`].
    pub signer: Verified,
    /// Its signer's URI: the custodian whose streams it's numbered on.
    pub custodian: String,
    /// When kiln vouches it took it (unix seconds): the time it's checked at.
    pub at: i64,
}

/// Checks a signed statement `bundle` kiln took at `at` (unix seconds: its own record of when,
/// never a time the statement claims): its signer's chain to kiln's root `root`, as of `at`
/// against `revocations`, and its signature over its PAE; then its payload's form. What it says
/// and who signed it, or why it isn't taken.
pub fn verify(bundle: &Value, root: &[u8], revocations: &[Revocation], at: i64) -> Result<Checked, Refusal> {
    let o = open(bundle)?;
    let chain: Vec<&[u8]> = o.chain.iter().map(Vec::as_slice).collect();
    let signer = chain::verify_signed(&chain, root, revocations, at, &to_sign(&o.payload_type, &o.payload), &o.signature)?;
    let statement: Statement = serde_json::from_slice(&o.payload).map_err(|e| Refusal::Malformed(format!("its payload isn't a statement: {e}")))?;
    statement.check().map_err(Refusal::Malformed)?;
    let custodian = signer.identity.uri()?;
    Ok(Checked { digest: digest(&o.payload_type, &o.payload), payload_type: o.payload_type, statement, signer, custodian, at })
}

/// A statement, and the delegations its signer acts under, nearest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Under {
    pub statement: Checked,
    pub through: Vec<Checked>,
}

impl fmt::Display for Under {
    /// Who signed, for whom, and through whom: "runner jupiter (…), under a delegation from agent
    /// matrix, epoch 3, for sam, of hyper-light".
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.statement.signer)?;
        for link in &self.through {
            write!(f, ", under a delegation from {}", link.signer)?;
        }
        Ok(())
    }
}

/// [`verify`], then the delegations its signer acts under, one under another from its `under`,
/// each found by its digest in kiln's records: `taken` gives the bundle kiln took under a digest
/// and when it took it. Each link is a delegation that verifies at the time kiln took it (a
/// certificate revoked since, as an agent's epoch is when it moves on, doesn't undo it), taken no
/// later than what acts under it, and naming as its `delegate` the signer below it; the last acts
/// under none. Refused naming the link that doesn't hold, and why.
pub fn verify_under(bundle: &Value, root: &[u8], revocations: &[Revocation], at: i64, taken: impl Fn(&str) -> Option<(Value, i64)>) -> Result<Under, Refusal> {
    let statement = verify(bundle, root, revocations, at)?;
    let mut through: Vec<Checked> = Vec::new();
    let mut next = statement.statement.under.clone();
    while let Some(link) = next {
        let n = through.len() + 1;
        if n > MAX_UNDER {
            return Err(Refusal::TooDeep);
        }
        let below = through.last().unwrap_or(&statement);
        let fail = |why| Refusal::Under { n, link: link.clone(), why };
        let (b, when) = taken(&link).ok_or_else(|| fail(Link::Unknown))?;
        let c = verify(&b, root, revocations, when).map_err(|e| fail(Link::Refused(Box::new(e))))?;
        if c.digest != link {
            return Err(fail(Link::Another(c.digest)));
        }
        if c.payload_type != PAYLOAD_DELEGATION {
            return Err(fail(Link::NotADelegation(c.payload_type)));
        }
        if when > below.at {
            return Err(fail(Link::TakenAfter { taken: chain::when(when), below: chain::when(below.at) }));
        }
        let named = c.statement.body.get("delegate").and_then(Value::as_str);
        if named != Some(below.custodian.as_str()) {
            return Err(fail(Link::NotTheDelegate { named: named.unwrap_or("no one").to_owned(), signer: below.custodian.clone() }));
        }
        next = c.statement.under.clone();
        through.push(c);
    }
    Ok(Under { statement, through })
}

/// Why a statement isn't taken.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("not a signed statement: {0}")]
    Malformed(String),
    #[error(transparent)]
    Signer(#[from] chain::Refusal),
    #[error("delegation {n} it acts under ({link}): {why}")]
    Under { n: usize, link: String, why: Link },
    #[error("it acts under more than {MAX_UNDER} delegations")]
    TooDeep,
}

/// Why a delegation a statement acts under doesn't hold.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Link {
    #[error("kiln has no record of it")]
    Unknown,
    #[error("it isn't taken: {0}")]
    Refused(Box<Refusal>),
    #[error("kiln's record under its digest is another statement ({0})")]
    Another(String),
    #[error("it's a {0} statement, not a delegation")]
    NotADelegation(String),
    #[error("kiln took it at {taken}, after what acts under it ({below})")]
    TakenAfter { taken: String, below: String },
    #[error("it delegates to {named}, not {signer}")]
    NotTheDelegate { named: String, signer: String },
}

/// A break in a custodian's numbering of one of its streams.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Gap {
    /// Numbers `from` to `to` that no statement has, below the highest one does.
    Missing { custodian: String, stream: String, from: u64, to: u64 },
    /// A number two or more statements have, by their digests.
    Twice { custodian: String, stream: String, seq: u64, digests: Vec<String> },
}

impl fmt::Display for Gap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Gap::Missing { custodian, stream, from, to } if from == to => write!(f, "{custodian}'s record {from} on {stream} is missing"),
            Gap::Missing { custodian, stream, from, to } => write!(f, "{custodian}'s records {from} to {to} on {stream} are missing"),
            Gap::Twice { custodian, stream, seq, digests } => write!(f, "{custodian} numbered {} statements {seq} on {stream}: {}", digests.len(), digests.join(", ")),
        }
    }
}

/// The gaps in each custodian's numbering of each of its streams among `checked`: numbers from 1
/// to the highest seen that no statement has, and numbers more than one statement has. The same
/// statement seen twice is one; a statement on no stream is no stream's.
pub fn gaps<'a>(checked: impl IntoIterator<Item = &'a Checked>) -> Vec<Gap> {
    let mut streams: BTreeMap<(&str, &str), BTreeMap<u64, BTreeSet<&str>>> = BTreeMap::new();
    for c in checked {
        if let (Some(stream), Some(seq)) = (c.statement.stream.as_deref(), c.statement.seq) {
            streams.entry((c.custodian.as_str(), stream)).or_default().entry(seq).or_default().insert(c.digest.as_str());
        }
    }
    let mut out = Vec::new();
    for ((custodian, stream), numbered) in streams {
        let mut next = 1;
        for (seq, digests) in numbered {
            if seq > next {
                out.push(Gap::Missing { custodian: custodian.to_owned(), stream: stream.to_owned(), from: next, to: seq - 1 });
            }
            if digests.len() > 1 {
                out.push(Gap::Twice { custodian: custodian.to_owned(), stream: stream.to_owned(), seq, digests: digests.into_iter().map(str::to_owned).collect() });
            }
            next = seq.saturating_add(1);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::chain::{Actor, Identity, fulcio};
    use rcgen::{BasicConstraints, CertificateParams, CustomExtension, IsCa, Issuer, KeyPair, KeyUsagePurpose, PublicKeyData as _, SanType, SerialNumber, SigningKey as _, date_time_ymd};

    /// 2026-09-21T14:13:20Z: inside every test leaf's validity (2026-01-01 to 2027-01-01).
    const AT: i64 = 1_790_000_000;
    /// 14:00 that day, the plan's example's.
    const T1400: i64 = AT - 800;

    const SAM: &str = "kiln://hyper-light/person/sam";
    const AGENT: &str = "kiln://hyper-light/person/sam/agent/matrix/epoch/3/machine/adacachy/instance/01a174ed";
    const JUPITER: &str = "kiln://hyper-light/person/ada/machine/jupiter/instance/01a18b00";
    const FEDORA: &str = "kiln://hyper-light/person/ada/machine/fedora-2/instance/01a174ed";

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

    /// An actor's certificate and key: what its holder signs with.
    struct Signer {
        der: Vec<u8>,
        key: KeyPair,
    }

    impl Signer {
        /// `s` as `payload_type`, signed: its bundle and its digest.
        fn sign(&self, payload_type: &str, s: &Statement) -> anyhow::Result<(Value, String)> {
            let p = payload(s)?;
            Ok((bundle(payload_type, &p, &self.key.sign(&to_sign(payload_type, &p))?, &self.der), digest(payload_type, &p)))
        }

        /// The SHA-256 of its key, as a revocation names it.
        fn key_id(&self, ca: &Ca) -> anyhow::Result<String> {
            Ok(chain::verify(&[&self.der], &ca.der, &[], AT)?.key)
        }
    }

    /// A certificate from `ca` for `uri`, with a new key (a leg's with the facts its kind needs).
    fn signer(ca: &Ca, uri: &str) -> anyhow::Result<Signer> {
        let mut p = CertificateParams::new(Vec::<String>::new())?;
        p.subject_alt_names = vec![SanType::URI(uri.try_into()?)];
        p.is_ca = IsCa::ExplicitNoCa;
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        p.not_before = date_time_ymd(2026, 1, 1);
        p.not_after = date_time_ymd(2027, 1, 1);
        let key = KeyPair::generate()?;
        let serial: Vec<u8> = std::iter::once(0x41).chain(Sha256::digest(key.der_bytes()).iter().take(8).copied()).collect();
        p.serial_number = Some(SerialNumber::from_slice(&serial));
        if uri.contains("/leg/") {
            for (member, value) in [(fulcio::SOURCE_REPOSITORY_DIGEST, "4c1e2a90d1f3b5a7c9e1f3a5b7c9d1e3f5a7b9c1"), (fulcio::BUILD_SIGNER_URI, "https://kiln.home/hyper-light/kiln/.kiln/ci.yml@refs/heads/main")] {
                let mut oid = fulcio::ARC.to_vec();
                oid.push(member);
                p.custom_extensions.push(CustomExtension::from_oid_content(&oid, chain::der_utf8_string(value).ok_or_else(|| anyhow::anyhow!("too long"))?));
            }
        }
        let der = p.signed_by(&key, &ca.issuer)?.der().to_vec();
        Ok(Signer { der, key })
    }

    fn sha(c: char) -> String {
        c.to_string().repeat(64)
    }

    fn subject(name: &str, c: char) -> Subject {
        Subject { name: name.into(), digest: BTreeMap::from([("sha256".to_owned(), sha(c))]) }
    }

    /// A custody record of `kind` about `what`, following `follows`, numbered `seq` on `stream`.
    fn custody(kind: &str, what: Subject, follows: &[&str], stream: &str, seq: u64) -> Statement {
        Statement {
            subject: vec![what],
            follows: follows.iter().map(|d| (*d).to_owned()).collect(),
            stream: Some(stream.into()),
            seq: Some(seq),
            ..Statement::new(json!({ "kind": kind }))
        }
    }

    /// A delegation to `to`, under `under`.
    fn delegation(to: &str, under: Option<&str>) -> Statement {
        Statement { under: under.map(str::to_owned), ..Statement::new(json!({ "delegate": to, "entitles": "custody of the workspace" })) }
    }

    /// `b` with the value at `pointer` replaced, its signature left as it was.
    fn altered(b: &Value, pointer: &str, value: Value) -> anyhow::Result<Value> {
        let mut b = b.clone();
        *b.pointer_mut(pointer).ok_or_else(|| anyhow::anyhow!("no {pointer}"))? = value;
        Ok(b)
    }

    #[test]
    fn w27_r03_a_statement_verifies_as_its_signer() -> anyhow::Result<()> {
        let ca = ca("kiln CA")?;
        let (ada, runner) = ("kiln://hyper-light/person/ada", "kiln://hyper-light/person/ada/machine/adacachy/instance/01a174ed");
        let leg = "kiln://hyper-light/person/ada/machine/adacachy/instance/01a174ed/run/1452/attempt/1/leg/test";
        for (uri, who, acts_for) in [
            (SAM, "sam of hyper-light", vec![]),
            (AGENT, "agent matrix, epoch 3, for sam, on runner adacachy (instance 01a174ed), of hyper-light", vec![SAM]),
            (JUPITER, "runner jupiter (instance 01a18b00), enrolled by ada, of hyper-light", vec![ada]),
            (leg, "leg test of run 1452 (attempt 1) at 4c1e2a9, on runner adacachy (instance 01a174ed), enrolled by ada, of hyper-light", vec![runner, ada]),
            ("kiln://hyper-light/server", "kiln server of hyper-light", vec![]),
            ("kiln://hyper-light/server/cd", "CD, for the kiln server of hyper-light", vec!["kiln://hyper-light/server"]),
        ] {
            let s = custody("handed-on", subject("workspace", 'a'), &[], "sync", 1);
            let (b, d) = signer(&ca, uri)?.sign(PAYLOAD_CUSTODY, &s)?;
            let c = verify(&b, &ca.der, &[], AT)?;
            assert_eq!(c.statement, s, "{uri}: it says what was signed");
            assert_eq!((c.digest.as_str(), c.payload_type.as_str(), c.custodian.as_str(), c.at), (d.as_str(), PAYLOAD_CUSTODY, uri, AT));
            assert_eq!(c.signer.to_string(), who);
            assert_eq!(c.signer.acts_for().iter().map(Identity::uri).collect::<Result<Vec<_>, _>>()?, acts_for, "{uri}");
        }
        // An agent's verifies as the agent, for its person, never as the person.
        let (b, _) = signer(&ca, AGENT)?.sign(PAYLOAD_CUSTODY, &custody("received", subject("workspace", 'a'), &[], "sync", 1))?;
        let c = verify(&b, &ca.der, &[], AT)?;
        assert!(!c.signer.identity.is_person("sam"));
        assert_eq!(c.signer.acts_for(), vec![Identity::new("hyper-light", Actor::Person { name: "sam".into() })]);
        Ok(())
    }

    /// The plan's example: the agent's statement kiln took at 14:00 verifies; after its epoch's
    /// revocation at 14:02, the same statement kiln took at 14:05 is refused, though it says it was
    /// made at 13:59.
    #[test]
    fn w27_r03_a_backdated_statement_is_judged_when_kiln_took_it() -> anyhow::Result<()> {
        let ca = ca("kiln CA")?;
        let agent = signer(&ca, AGENT)?;
        let revoked = [Revocation { serial: None, key: Some(agent.key_id(&ca)?), at: T1400 + 120, reason: "the session moved to epoch 4".into(), by: "kiln".into() }];
        let s = Statement { subject: vec![subject("workspace", 'a')], ..Statement::new(json!({ "kind": "handed-on", "made": T1400 - 60 })) };
        let (b, _) = agent.sign(PAYLOAD_CUSTODY, &s)?;
        assert!(verify(&b, &ca.der, &revoked, T1400).is_ok(), "taken at 14:00, before the revocation");
        let got = verify(&b, &ca.der, &revoked, T1400 + 300);
        assert!(matches!(&got, Err(Refusal::Signer(chain::Refusal::Revoked { reason, .. })) if reason == "the session moved to epoch 4"), "{got:?}");
        assert!(got.is_err_and(|e| e.to_string().contains("revoked at 2026-09-21T14:02:00Z") && e.to_string().contains("checked at 2026-09-21T14:05:00Z")));
        Ok(())
    }

    #[test]
    fn w27_r03_a_changed_statement_or_another_root_is_refused() -> anyhow::Result<()> {
        let ca = ca("kiln CA")?;
        let (agent, sam) = (signer(&ca, AGENT)?, signer(&ca, SAM)?);
        let (b, _) = agent.sign(PAYLOAD_CUSTODY, &custody("handed-on", subject("workspace", 'a'), &[], "sync", 1))?;
        let agents = "agent matrix, epoch 3, for sam, on runner adacachy (instance 01a174ed), of hyper-light";
        // Its payload changed (another workspace), or its payload type: the signature isn't the agent's.
        let other = payload(&custody("handed-on", subject("workspace", 'b'), &[], "sync", 1))?;
        for changed in [altered(&b, "/dsseEnvelope/payload", json!(b64(&other)))?, altered(&b, "/dsseEnvelope/payloadType", json!(PAYLOAD_DELEGATION))?] {
            let got = verify(&changed, &ca.der, &[], AT);
            assert!(matches!(&got, Err(Refusal::Signer(chain::Refusal::BadSignature { who })) if who == agents), "{got:?}");
        }
        // sam's certificate on the agent's signature: not sam's either.
        let got = verify(&altered(&b, "/verificationMaterial/certificate/rawBytes", json!(b64(&sam.der)))?, &ca.der, &[], AT);
        assert!(matches!(&got, Err(Refusal::Signer(chain::Refusal::BadSignature { who })) if who == "sam of hyper-light"), "{got:?}");
        // Certified by another CA: not kiln's.
        let (b, _) = signer(&self::ca("someone else's CA")?, AGENT)?.sign(PAYLOAD_CUSTODY, &custody("handed-on", subject("workspace", 'a'), &[], "sync", 1))?;
        assert!(matches!(verify(&b, &ca.der, &[], AT), Err(Refusal::Signer(chain::Refusal::NotIssued { .. }))));
        Ok(())
    }

    #[test]
    fn w27_r03_a_statements_digest_is_its_pae_whoever_signs_it() -> anyhow::Result<()> {
        let ca = ca("kiln CA")?;
        let s = custody("passed-on", subject("pack", 'c'), &[], "reach", 7);
        let p = payload(&s)?;
        let (once, d) = signer(&ca, JUPITER)?.sign(PAYLOAD_CUSTODY, &s)?;
        let (again, d2) = signer(&ca, JUPITER)?.sign(PAYLOAD_CUSTODY, &s)?;
        assert_ne!(once, again, "signed again, by another key");
        assert_eq!((verify(&once, &ca.der, &[], AT)?.digest, verify(&again, &ca.der, &[], AT)?.digest), (d.clone(), d2));
        assert_eq!(digest(PAYLOAD_CUSTODY, &p), d, "the same statement");
        // SHA-256 over DSSE's PAE, "DSSEv1 <length> <type> <length> <payload>".
        let pae = [format!("DSSEv1 {} {PAYLOAD_CUSTODY} {} ", PAYLOAD_CUSTODY.len(), p.len()).into_bytes(), p.clone()].concat();
        assert_eq!(d, format!("sha256:{}", chain::hex(&Sha256::digest(&pae))));
        assert_ne!(digest(PAYLOAD_DELEGATION, &p), d, "another payload type is another statement");
        Ok(())
    }

    #[test]
    fn w27_r03_malformed_statements_are_refused_saying_why() -> anyhow::Result<()> {
        let ca = ca("kiln CA")?;
        let sam = signer(&ca, SAM)?;
        let follows = format!("sha256:{}", sha('e'));
        let good = custody("taken", subject("image", 'd'), &[follows.as_str()], "deploy", 2);
        let with_digest = |alg: &str, hex: &str| Statement { subject: vec![Subject { name: "image".into(), digest: BTreeMap::from([(alg.to_owned(), hex.to_owned())]) }], ..good.clone() };
        for (s, why) in [
            (Statement { v: 2, ..good.clone() }, "version 2"),
            (Statement { subject: vec![Subject { name: "image".into(), digest: BTreeMap::new() }], ..good.clone() }, "\"image\" has no digest"),
            (with_digest("sha256", "abc"), "malformed sha256 digest"),
            (with_digest("gitCommit", "4C1E2A90"), "malformed gitCommit digest"),
            (Statement { follows: vec![sha('e')], ..good.clone() }, "isn't a statement's digest"),
            (Statement { under: Some("sha256:xyz".into()), ..good.clone() }, "isn't a statement's digest"),
            (Statement { follows: vec![follows.clone(), follows.clone()], ..good.clone() }, "twice"),
            (Statement { seq: None, ..good.clone() }, "no number on it"),
            (Statement { stream: None, ..good.clone() }, "names no stream"),
            (Statement { seq: Some(0), ..good.clone() }, "numbered from 1"),
            (Statement { stream: Some(String::new()), ..good.clone() }, "isn't a stream's name"),
        ] {
            // kiln won't sign it...
            let got = payload(&s);
            assert!(matches!(&got, Err(Refusal::Malformed(w)) if w.contains(why)), "{why}: {got:?}");
            // ... nor take it, signed anyway.
            let raw = serde_json::to_vec(&s)?;
            let got = verify(&bundle(PAYLOAD_CUSTODY, &raw, &sam.key.sign(&to_sign(PAYLOAD_CUSTODY, &raw))?, &sam.der), &ca.der, &[], AT);
            assert!(matches!(&got, Err(Refusal::Malformed(w)) if w.contains(why)), "{why}: {got:?}");
        }
        assert!(with_digest("gitCommit", "4c1e2a90d1f3b5a7c9e1f3a5b7c9d1e3f5a7b9c1").check().is_ok(), "other digests than SHA-256 are taken");
        // A signed payload that isn't a statement.
        let raw = b"[1, 2]";
        let got = verify(&bundle(PAYLOAD_CUSTODY, raw, &sam.key.sign(&to_sign(PAYLOAD_CUSTODY, raw))?, &sam.der), &ca.der, &[], AT);
        assert!(matches!(&got, Err(Refusal::Malformed(w)) if w.contains("isn't a statement")), "{got:?}");
        // Bundles that aren't one signed statement.
        let (b, _) = sam.sign(PAYLOAD_CUSTODY, &good)?;
        let sig = b.pointer("/dsseEnvelope/signatures/0").cloned().unwrap_or_default();
        for (bad, why) in [
            (altered(&b, "/dsseEnvelope/signatures", json!([sig.clone(), sig]))?, "2 signatures"),
            (altered(&b, "/dsseEnvelope/signatures", json!([]))?, "0 signatures"),
            (altered(&b, "/verificationMaterial", json!({}))?, "no certificate"),
            (altered(&b, "/dsseEnvelope/payload", json!("not base64!"))?, "payload isn't base64"),
            (altered(&b, "/mediaType", json!("application/json"))?, "isn't a Sigstore bundle"),
        ] {
            let got = verify(&bad, &ca.der, &[], AT);
            assert!(matches!(&got, Err(Refusal::Malformed(w)) if w.contains(why)), "{why}: {got:?}");
        }
        Ok(())
    }

    /// Who signed, for whom, through whom: sam entitles the matrix agent, the agent entitles runner
    /// jupiter to take its workspace, and jupiter's receipt, following the agent's hand-on, acts
    /// under the agent's delegation.
    #[test]
    fn w27_r08_under_walks_each_delegation_to_one_under_none() -> anyhow::Result<()> {
        let ca = ca("kiln CA")?;
        let (sam, agent, jupiter) = (signer(&ca, SAM)?, signer(&ca, AGENT)?, signer(&ca, JUPITER)?);
        let (b, to_agent) = sam.sign(PAYLOAD_DELEGATION, &delegation(AGENT, None))?;
        let mut kept = BTreeMap::from([(to_agent.clone(), (b, AT - 60))]);
        let (b, to_jupiter) = agent.sign(PAYLOAD_DELEGATION, &delegation(JUPITER, Some(&to_agent)))?;
        kept.insert(to_jupiter.clone(), (b, AT - 30));
        let (_, hand_on) = agent.sign(PAYLOAD_CUSTODY, &custody("handed-on", subject("workspace", 'a'), &[], "sync", 1))?;
        let receipt = Statement { under: Some(to_jupiter.clone()), ..custody("received", subject("workspace", 'a'), &[hand_on.as_str()], "sync", 1) };
        let (b, _) = jupiter.sign(PAYLOAD_CUSTODY, &receipt)?;
        let u = verify_under(&b, &ca.der, &[], AT, |d| kept.get(d).cloned())?;
        assert_eq!(u.statement.statement.follows, vec![hand_on]);
        assert_eq!(u.through.iter().map(|c| (c.digest.as_str(), c.custodian.as_str(), c.at)).collect::<Vec<_>>(), vec![(to_jupiter.as_str(), AGENT, AT - 30), (to_agent.as_str(), SAM, AT - 60)]);
        assert_eq!(
            u.to_string(),
            "runner jupiter (instance 01a18b00), enrolled by ada, of hyper-light, under a delegation from agent matrix, epoch 3, for sam, on runner adacachy (instance 01a174ed), of hyper-light, under a delegation from sam of hyper-light"
        );
        // The agent's epoch revoked after it delegated (its session moved on) doesn't undo the delegation.
        let moved = [Revocation { serial: None, key: Some(agent.key_id(&ca)?), at: AT - 10, reason: "moved to epoch 4".into(), by: "kiln".into() }];
        assert_eq!(verify_under(&b, &ca.der, &moved, AT, |d| kept.get(d).cloned())?.through.len(), 2);
        // Under none: no links, and kiln's records aren't asked.
        let (b, _) = jupiter.sign(PAYLOAD_CUSTODY, &custody("received", subject("workspace", 'a'), &[], "sync", 2))?;
        assert_eq!(verify_under(&b, &ca.der, &[], AT, |_| None)?.through, vec![]);
        // As deep as kiln goes, and no deeper.
        for (links, holds) in [(MAX_UNDER, true), (MAX_UNDER + 1, false)] {
            let mut kept = BTreeMap::new();
            let mut under: Option<String> = None;
            for _ in 0..links {
                let (b, d) = jupiter.sign(PAYLOAD_DELEGATION, &delegation(JUPITER, under.as_deref()))?;
                kept.insert(d.clone(), (b, AT - 1));
                under = Some(d);
            }
            let (b, _) = jupiter.sign(PAYLOAD_CUSTODY, &Statement { under, ..custody("received", subject("workspace", 'a'), &[], "sync", 3) })?;
            let got = verify_under(&b, &ca.der, &[], AT, |d| kept.get(d).cloned());
            assert_eq!(got.is_ok(), holds, "{links} links: {got:?}");
            assert!(holds || matches!(got, Err(Refusal::TooDeep)));
        }
        Ok(())
    }

    #[test]
    fn w27_r08_a_broken_delegation_is_refused_naming_its_link() -> anyhow::Result<()> {
        let ca = ca("kiln CA")?;
        let (sam, agent, jupiter) = (signer(&ca, SAM)?, signer(&ca, AGENT)?, signer(&ca, JUPITER)?);
        let (to_agent_b, to_agent) = sam.sign(PAYLOAD_DELEGATION, &delegation(AGENT, None))?;
        let (to_jupiter_b, to_jupiter) = agent.sign(PAYLOAD_DELEGATION, &delegation(JUPITER, Some(&to_agent)))?;
        // jupiter's receipt under `link`, checked against `kept` and `revocations`.
        let receipt = |link: &str| -> anyhow::Result<Value> {
            Ok(jupiter.sign(PAYLOAD_CUSTODY, &Statement { under: Some(link.to_owned()), ..custody("received", subject("workspace", 'a'), &[], "sync", 1) })?.0)
        };
        let check = |b: &Value, kept: &[(&String, &Value, i64)], revocations: &[Revocation]| {
            verify_under(b, &ca.der, revocations, AT, |d| kept.iter().find(|(k, _, _)| k.as_str() == d).map(|(_, b, at)| ((*b).clone(), *at)))
        };
        let r = receipt(&to_jupiter)?;
        assert!(check(&r, &[(&to_jupiter, &to_jupiter_b, AT - 30), (&to_agent, &to_agent_b, AT - 60)], &[]).is_ok());

        // kiln has no record of the first link, then of the second.
        let got = check(&r, &[], &[]);
        assert!(matches!(&got, Err(Refusal::Under { n: 1, link, why: Link::Unknown }) if *link == to_jupiter), "{got:?}");
        let got = check(&r, &[(&to_jupiter, &to_jupiter_b, AT - 30)], &[]);
        assert!(matches!(&got, Err(Refusal::Under { n: 2, link, why: Link::Unknown }) if *link == to_agent), "{got:?}");
        assert!(got.is_err_and(|e| e.to_string() == format!("delegation 2 it acts under ({to_agent}): kiln has no record of it")));

        // The first link delegates to another runner.
        let (to_fedora_b, to_fedora) = agent.sign(PAYLOAD_DELEGATION, &delegation(FEDORA, Some(&to_agent)))?;
        let got = check(&receipt(&to_fedora)?, &[(&to_fedora, &to_fedora_b, AT - 30), (&to_agent, &to_agent_b, AT - 60)], &[]);
        assert!(matches!(&got, Err(Refusal::Under { n: 1, why: Link::NotTheDelegate { named, signer }, .. }) if named == FEDORA && signer == JUPITER), "{got:?}");

        // The second link (sam's) delegates to another agent than the one that delegated on.
        let (to_other_b, to_other) = sam.sign(PAYLOAD_DELEGATION, &delegation("kiln://hyper-light/person/sam/agent/other/epoch/1", None))?;
        let (via_other_b, via_other) = agent.sign(PAYLOAD_DELEGATION, &delegation(JUPITER, Some(&to_other)))?;
        let got = check(&receipt(&via_other)?, &[(&via_other, &via_other_b, AT - 30), (&to_other, &to_other_b, AT - 60)], &[]);
        assert!(matches!(&got, Err(Refusal::Under { n: 2, link, why: Link::NotTheDelegate { signer, .. } }) if *link == to_other && signer == AGENT), "{got:?}");

        // A custody record named as a delegation.
        let (record_b, record) = agent.sign(PAYLOAD_CUSTODY, &Statement::new(json!({ "delegate": JUPITER })))?;
        let got = check(&receipt(&record)?, &[(&record, &record_b, AT - 30)], &[]);
        assert!(matches!(&got, Err(Refusal::Under { n: 1, why: Link::NotADelegation(t), .. }) if t == PAYLOAD_CUSTODY), "{got:?}");

        // Taken after what acts under it: no entitling after the fact.
        let got = check(&r, &[(&to_jupiter, &to_jupiter_b, AT + 60), (&to_agent, &to_agent_b, AT - 60)], &[]);
        assert!(matches!(&got, Err(Refusal::Under { n: 1, why: Link::TakenAfter { .. }, .. })), "{got:?}");
        let got = check(&r, &[(&to_jupiter, &to_jupiter_b, AT - 30), (&to_agent, &to_agent_b, AT - 20)], &[]);
        assert!(matches!(&got, Err(Refusal::Under { n: 2, why: Link::TakenAfter { .. }, .. })), "the second, after the first: {got:?}");

        // Signed by an identity revoked by the time kiln took it.
        let lost = [Revocation { serial: None, key: Some(agent.key_id(&ca)?), at: AT - 45, reason: "key lost".into(), by: "sam".into() }];
        let got = check(&r, &[(&to_jupiter, &to_jupiter_b, AT - 30), (&to_agent, &to_agent_b, AT - 60)], &lost);
        assert!(matches!(&got, Err(Refusal::Under { n: 1, why: Link::Refused(e), .. }) if matches!(**e, Refusal::Signer(chain::Refusal::Revoked { .. }))), "{got:?}");

        // kiln's record under its digest is another statement.
        let got = check(&r, &[(&to_jupiter, &to_agent_b, AT - 30)], &[]);
        assert!(matches!(&got, Err(Refusal::Under { n: 1, why: Link::Another(d), .. }) if *d == to_agent), "{got:?}");
        Ok(())
    }

    #[test]
    fn w27_r08_gaps_name_each_missing_record_and_each_number_used_twice() -> anyhow::Result<()> {
        let ca = ca("kiln CA")?;
        let (jupiter, agent) = (signer(&ca, JUPITER)?, signer(&ca, AGENT)?);
        let take = |s: &Signer, stream: &str, seq: u64, c: char| -> anyhow::Result<Checked> {
            Ok(verify(&s.sign(PAYLOAD_CUSTODY, &custody("passed-on", subject("pack", c), &[], stream, seq))?.0, &ca.der, &[], AT)?)
        };
        let unnumbered = verify(&jupiter.sign(PAYLOAD_CUSTODY, &Statement::new(json!({ "kind": "taken" })))?.0, &ca.der, &[], AT)?;
        let (twice_a, twice_b) = (take(&agent, "reach", 1, 'e')?, take(&agent, "reach", 1, 'f')?);
        let records = vec![
            take(&jupiter, "reach", 1, 'a')?,
            take(&jupiter, "reach", 2, 'b')?,
            take(&jupiter, "reach", 4, 'c')?,
            take(&jupiter, "reach", 4, 'c')?,
            take(&jupiter, "sync", 3, 'd')?,
            twice_a.clone(),
            twice_b.clone(),
            take(&agent, "reach", 2, '1')?,
            unnumbered,
        ];
        let mut digests = vec![twice_a.digest, twice_b.digest];
        digests.sort();
        let got = gaps(&records);
        assert_eq!(
            got,
            vec![
                Gap::Missing { custodian: JUPITER.into(), stream: "reach".into(), from: 3, to: 3 },
                Gap::Missing { custodian: JUPITER.into(), stream: "sync".into(), from: 1, to: 2 },
                Gap::Twice { custodian: AGENT.into(), stream: "reach".into(), seq: 1, digests: digests.clone() },
            ]
        );
        let said: Vec<String> = got.iter().map(Gap::to_string).collect();
        assert_eq!(
            said,
            vec![
                format!("{JUPITER}'s record 3 on reach is missing"),
                format!("{JUPITER}'s records 1 to 2 on sync are missing"),
                format!("{AGENT} numbered 2 statements 1 on reach: {}", digests.join(", ")),
            ]
        );
        assert_eq!(gaps(records.iter().take(2)), vec![], "1 and 2: no gap");
        Ok(())
    }
}
