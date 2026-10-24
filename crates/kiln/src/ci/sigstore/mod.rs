//! Sigstore bundles verified (docs/ci.md W18): keyless signatures, as GitHub's
//! `actions/attest-build-provenance` and kiln's own keyless signing make them, checked as
//! Sigstore's clients check them:
//! - the certificate chains to a Fulcio of the trusted root, every certificate in the chain valid
//!   when the log took the signature, the leaf for code signing, with a CT log's timestamp of it
//!   (an SCT) when the trusted root has CT logs;
//! - the DSSE envelope's signature is the certificate's key's;
//! - a transparency log (Rekor) entry is of this envelope, signature and certificate: the log's
//!   promise (its signed entry timestamp, which says when) verifies, and its inclusion proof leads
//!   to a checkpoint the log signed;
//! - the certificate names the identity asked for: its OIDC issuer, its subject (a workflow, for
//!   GitHub's), and Fulcio's claims (the source repository, its ref…);
//! - the statement names the artifact's digest.
//!
//! The trusted root is a `trusted_root.json` (`[ci.attest] trusted_root`, `--trusted-root`), or
//! Sigstore's public instance's, through its TUF repository ([`tuf`]). `kiln attest verify` checks
//! an artifact's bundles, from a file or GitHub's attestations API.

use std::collections::BTreeMap;
use std::path::PathBuf;

use aws_lc_rs::signature::{self as sig, UnparsedPublicKey, VerificationAlgorithm};
use base64::Engine as _;
use serde_json::{Value, json};
use x509_parser::asn1_rs::Oid;
use x509_parser::prelude::*;

pub mod tuf;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// GitHub Actions' OIDC issuer: Fulcio's certificates for its workflows name it.
pub const GITHUB_ISSUER: &str = "https://token.actions.githubusercontent.com";

/// The most a bundle from GitHub may be, compressed or not.
const MAX_BUNDLE: usize = 16 << 20;

/// Fulcio's certificate extensions (its OIDs, 1.3.6.1.4.1.57264.1.*), by the names its
/// documentation gives them.
const CLAIMS: &[(&str, &str)] = &[
    ("1.3.6.1.4.1.57264.1.9", "build_signer_uri"),
    ("1.3.6.1.4.1.57264.1.10", "build_signer_digest"),
    ("1.3.6.1.4.1.57264.1.11", "runner_environment"),
    ("1.3.6.1.4.1.57264.1.12", "source_repository_uri"),
    ("1.3.6.1.4.1.57264.1.13", "source_repository_digest"),
    ("1.3.6.1.4.1.57264.1.14", "source_repository_ref"),
    ("1.3.6.1.4.1.57264.1.15", "source_repository_identifier"),
    ("1.3.6.1.4.1.57264.1.16", "source_repository_owner_uri"),
    ("1.3.6.1.4.1.57264.1.17", "source_repository_owner_identifier"),
    ("1.3.6.1.4.1.57264.1.18", "build_config_uri"),
    ("1.3.6.1.4.1.57264.1.19", "build_config_digest"),
    ("1.3.6.1.4.1.57264.1.20", "build_trigger"),
    ("1.3.6.1.4.1.57264.1.21", "run_invocation_uri"),
    ("1.3.6.1.4.1.57264.1.22", "source_repository_visibility_at_signing"),
    ("1.3.6.1.4.1.57264.1.23", "deployment_environment"),
];
/// The issuer, as Fulcio wrote it first (its bytes) and now (a DER UTF8String).
const ISSUER_V1: &str = "1.3.6.1.4.1.57264.1.1";
const ISSUER_V2: &str = "1.3.6.1.4.1.57264.1.8";
/// RFC 6962's embedded SCT list, as DER: its OID's tag, length and value.
const SCT_LIST_OID: &[u8] = &[0x06, 0x0A, 0x2B, 0x06, 0x01, 0x04, 0x01, 0xD6, 0x79, 0x02, 0x04, 0x02];

const OID_EC: &str = "1.2.840.10045.2.1";
const OID_P256: &str = "1.2.840.10045.3.1.7";
const OID_P384: &str = "1.3.132.0.34";
const OID_ED25519: &str = "1.3.101.112";
const OID_RSA: &str = "1.2.840.113549.1.1.1";

fn sha256(b: &[u8]) -> Vec<u8> {
    aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, b).as_ref().to_vec()
}

// ------------------------------------------------------------------ keys

/// A public key, from its SubjectPublicKeyInfo: what verifying needs of it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Key {
    /// An uncompressed point.
    P256(Vec<u8>),
    P384(Vec<u8>),
    Ed25519(Vec<u8>),
    /// PKCS#1's RSAPublicKey.
    Rsa(Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hash {
    Sha256,
    Sha384,
}

/// How a signature was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Alg {
    Ecdsa(Hash),
    Ed25519,
    RsaPkcs1(Hash),
}

impl Alg {
    /// A certificate's signature algorithm, by its OID.
    fn of_oid(oid: &str) -> anyhow::Result<Alg> {
        Ok(match oid {
            "1.2.840.10045.4.3.2" => Alg::Ecdsa(Hash::Sha256),
            "1.2.840.10045.4.3.3" => Alg::Ecdsa(Hash::Sha384),
            "1.3.101.112" => Alg::Ed25519,
            "1.2.840.113549.1.1.11" => Alg::RsaPkcs1(Hash::Sha256),
            "1.2.840.113549.1.1.12" => Alg::RsaPkcs1(Hash::Sha384),
            other => anyhow::bail!("a certificate signed with {other}: kiln verifies ECDSA (SHA-256, SHA-384), Ed25519 and RSA (PKCS #1)"),
        })
    }
}

impl Key {
    fn from_spki(spki: &SubjectPublicKeyInfo<'_>) -> anyhow::Result<Key> {
        let bits = spki.subject_public_key.data.to_vec();
        Ok(match spki.algorithm.algorithm.to_id_string().as_str() {
            OID_EC => {
                let curve = spki.algorithm.parameters.as_ref().and_then(|p| Oid::try_from(p).ok()).map(|o| o.to_id_string()).unwrap_or_default();
                match curve.as_str() {
                    OID_P256 => Key::P256(bits),
                    OID_P384 => Key::P384(bits),
                    other => anyhow::bail!("an EC key on curve {other}: kiln verifies P-256 and P-384"),
                }
            }
            OID_ED25519 => Key::Ed25519(bits),
            OID_RSA => Key::Rsa(bits),
            other => anyhow::bail!("a {other} key"),
        })
    }

    /// A key from its SubjectPublicKeyInfo's DER.
    fn from_der(der: &[u8]) -> anyhow::Result<Key> {
        let (_, spki) = SubjectPublicKeyInfo::from_der(der).map_err(|e| anyhow::anyhow!("a public key that isn't a DER SubjectPublicKeyInfo: {e}"))?;
        Key::from_spki(&spki)
    }

    /// How Sigstore signs with this key: P-256 with SHA-256, P-384 with SHA-384.
    fn natural(&self) -> Alg {
        match self {
            Key::P256(_) => Alg::Ecdsa(Hash::Sha256),
            Key::P384(_) => Alg::Ecdsa(Hash::Sha384),
            Key::Ed25519(_) => Alg::Ed25519,
            Key::Rsa(_) => Alg::RsaPkcs1(Hash::Sha256),
        }
    }

    /// Whether `signature` (DER, for ECDSA) is this key's over `msg`, made as `alg` says.
    fn verifies(&self, alg: Alg, msg: &[u8], signature: &[u8]) -> bool {
        let (verifier, bytes): (&'static dyn VerificationAlgorithm, &[u8]) = match (self, alg) {
            (Key::P256(b), Alg::Ecdsa(Hash::Sha256)) => (&sig::ECDSA_P256_SHA256_ASN1, b),
            (Key::P256(b), Alg::Ecdsa(Hash::Sha384)) => (&sig::ECDSA_P256_SHA384_ASN1, b),
            (Key::P384(b), Alg::Ecdsa(Hash::Sha256)) => (&sig::ECDSA_P384_SHA256_ASN1, b),
            (Key::P384(b), Alg::Ecdsa(Hash::Sha384)) => (&sig::ECDSA_P384_SHA384_ASN1, b),
            (Key::Ed25519(b), Alg::Ed25519) => (&sig::ED25519, b),
            (Key::Rsa(b), Alg::RsaPkcs1(Hash::Sha256)) => (&sig::RSA_PKCS1_2048_8192_SHA256, b),
            (Key::Rsa(b), Alg::RsaPkcs1(Hash::Sha384)) => (&sig::RSA_PKCS1_2048_8192_SHA384, b),
            _ => return false,
        };
        UnparsedPublicKey::new(verifier, bytes).verify(msg, signature).is_ok()
    }
}

// ------------------------------------------------------------------ the trusted root

/// When a key or CA is trusted: from `start` on, until `end` when it has one (unix seconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Window {
    start: i64,
    end: Option<i64>,
}

impl Window {
    fn holds(&self, t: i64) -> bool {
        t >= self.start && self.end.is_none_or(|e| t <= e)
    }
}

/// A transparency log or CT log: its id (the SHA-256 of its key) and key.
#[derive(Debug, Clone)]
struct Log {
    id: Vec<u8>,
    key: Key,
    valid: Window,
    url: String,
}

/// A Fulcio: its chain (DER), the issuing certificate first and the root last.
#[derive(Debug, Clone)]
struct Ca {
    certs: Vec<Vec<u8>>,
    valid: Window,
    uri: String,
}

/// Sigstore's trust (a `trusted_root.json`, `application/vnd.dev.sigstore.trustedroot+json`): the
/// Fulcios that certify signing keys, the transparency logs that log signatures, the CT logs that
/// log certificates, and when each is trusted.
#[derive(Debug, Clone)]
pub struct TrustedRoot {
    cas: Vec<Ca>,
    tlogs: Vec<Log>,
    ctlogs: Vec<Log>,
}

fn rfc3339(s: &str) -> anyhow::Result<i64> {
    Ok(::time::OffsetDateTime::parse(s, &::time::format_description::well_known::Rfc3339).map_err(|e| anyhow::anyhow!("{s:?}: {e}"))?.unix_timestamp())
}

fn window(v: Option<&Value>) -> anyhow::Result<Window> {
    let at = |k: &str| v.and_then(|w| w.get(k)).and_then(Value::as_str).map(rfc3339).transpose();
    Ok(Window { start: at("start")?.unwrap_or(i64::MIN), end: at("end")? })
}

impl TrustedRoot {
    pub fn from_json(bytes: &[u8]) -> anyhow::Result<TrustedRoot> {
        let v: Value = serde_json::from_slice(bytes).map_err(|e| anyhow::anyhow!("the trusted root isn't JSON: {e}"))?;
        let media = v.get("mediaType").and_then(Value::as_str).unwrap_or_default();
        anyhow::ensure!(media.starts_with("application/vnd.dev.sigstore.trustedroot"), "not a Sigstore trusted root (its mediaType is {media:?})");
        // A log or CA kiln can't read (a kind of key it doesn't know) is passed over: the rest
        // still verify what they signed.
        let logs = |field: &str| -> Vec<Log> {
            v.get(field)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|l| {
                    let read = || -> anyhow::Result<Log> {
                        let id = B64.decode(l.pointer("/logId/keyId").and_then(Value::as_str).unwrap_or_default())?;
                        let der = B64.decode(l.pointer("/publicKey/rawBytes").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("no key"))?)?;
                        Ok(Log { id, key: Key::from_der(&der)?, valid: window(l.pointer("/publicKey/validFor"))?, url: l.get("baseUrl").and_then(Value::as_str).unwrap_or_default().to_owned() })
                    };
                    read().map_err(|e| tracing::debug!(field, error = %format!("{e:#}"), "a log of the trusted root passed over")).ok()
                })
                .collect()
        };
        let cas = v
            .get("certificateAuthorities")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|c| {
                let certs: Vec<Vec<u8>> = c.pointer("/certChain/certificates").and_then(Value::as_array)?.iter().filter_map(|x| B64.decode(x.get("rawBytes")?.as_str()?).ok()).collect();
                Some(Ca { certs, valid: window(c.get("validFor")).ok()?, uri: c.get("uri").and_then(Value::as_str).unwrap_or_default().to_owned() })
            })
            .filter(|c| !c.certs.is_empty())
            .collect();
        Ok(TrustedRoot { cas, tlogs: logs("tlogs"), ctlogs: logs("ctlogs") })
    }
}

// ------------------------------------------------------------------ identities

/// What a certificate's subject must be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subject {
    Exact(String),
    Prefix(String),
}

/// Who must have signed: the certificate's OIDC issuer, exactly; its subject (a workflow's URI,
/// an email), exactly, by prefix, or any; and Fulcio's claims, each exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub issuer: String,
    pub subject: Option<Subject>,
    pub claims: Vec<(&'static str, String)>,
}

impl Identity {
    /// A workflow of GitHub repository `repo` (`owner/name`) built it, from its source: the
    /// signing workflow `workflow` (`.github/workflows/release.yml` of `repo`, or
    /// `owner/name/.github/workflows/…`, a reusable one elsewhere) when given, else any of
    /// `repo`'s; and from `git_ref` (`refs/heads/main`) when given.
    pub fn github(repo: &str, workflow: Option<&str>, git_ref: Option<&str>) -> Identity {
        let repo = repo.trim_matches('/');
        let subject = match workflow.map(|w| w.trim_matches('/')) {
            Some(w) if w.starts_with(".github/") => Subject::Prefix(format!("https://github.com/{repo}/{w}@")),
            Some(w) => Subject::Prefix(format!("https://github.com/{w}@")),
            None => Subject::Prefix(format!("https://github.com/{repo}/")),
        };
        let mut claims = vec![("source_repository_uri", format!("https://github.com/{repo}"))];
        if let Some(r) = git_ref {
            claims.push(("source_repository_ref", r.to_owned()));
        }
        Identity { issuer: GITHUB_ISSUER.to_owned(), subject: Some(subject), claims }
    }
}

/// What a verified bundle says, and who signed it.
#[derive(Debug, Clone, PartialEq)]
pub struct Verified {
    /// The in-toto statement.
    pub statement: Value,
    /// The certificate's OIDC issuer and subject, and Fulcio's claims by name.
    pub issuer: String,
    pub subject: String,
    pub claims: BTreeMap<&'static str, String>,
    /// When the log took the signature (its signed entry timestamp's time, unix seconds), and
    /// the entry's index there.
    pub logged_at: i64,
    pub log_index: i64,
    pub log: String,
}

impl Verified {
    pub fn predicate_type(&self) -> &str {
        self.statement.get("predicateType").and_then(Value::as_str).unwrap_or_default()
    }

    pub fn to_json(&self) -> Value {
        json!({
            "predicate_type": self.predicate_type(),
            "issuer": self.issuer, "subject": self.subject, "claims": self.claims,
            "logged_at": self.logged_at, "log": self.log, "log_index": self.log_index,
            "statement": self.statement,
        })
    }
}

// ------------------------------------------------------------------ verifying

/// Verifies a Sigstore bundle (v0.1 to v0.3) of a DSSE envelope against `root`: signed by
/// `identity`, and, when `digest` (`(algorithm, hex)`) is given, about an artifact with that
/// digest. What it says and who signed it, or why it doesn't verify.
pub fn verify(bundle: &Value, root: &TrustedRoot, identity: &Identity, digest: Option<(&str, &str)>) -> anyhow::Result<Verified> {
    let media = bundle.get("mediaType").and_then(Value::as_str).unwrap_or_default();
    anyhow::ensure!(media.starts_with("application/vnd.dev.sigstore.bundle"), "not a Sigstore bundle (its mediaType is {media:?})");
    // v0.1's entries need only the log's promise; later ones its inclusion proof too.
    let v01 = media.ends_with("version=0.1");
    let vm = bundle.get("verificationMaterial").ok_or_else(|| anyhow::anyhow!("the bundle has no verification material"))?;
    let leaf_der = vm
        .pointer("/certificate/rawBytes")
        .or_else(|| vm.pointer("/x509CertificateChain/certificates/0/rawBytes"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("the bundle has no certificate: it was signed with a key, not keyless"))
        .and_then(|b| Ok(B64.decode(b)?))?;
    let env = bundle.get("dsseEnvelope").ok_or_else(|| anyhow::anyhow!("the bundle signs no DSSE envelope: kiln verifies attestations"))?;
    let payload_type = env.get("payloadType").and_then(Value::as_str).unwrap_or_default();
    let payload = B64.decode(env.get("payload").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("the envelope has no payload"))?)?;
    let sigs = env.get("signatures").and_then(Value::as_array).cloned().unwrap_or_default();
    anyhow::ensure!(sigs.len() == 1, "the envelope has {} signatures, not one", sigs.len());
    let signature = B64.decode(sigs.first().and_then(|s| s.get("sig")).and_then(Value::as_str).unwrap_or_default())?;

    // The log's entry of it, and when the log took it.
    let mut why = Vec::new();
    let mut logged = None;
    for entry in vm.get("tlogEntries").and_then(Value::as_array).into_iter().flatten() {
        match logged_entry(entry, root, !v01, &payload, &signature, &leaf_der) {
            Ok(l) => {
                logged = Some(l);
                break;
            }
            Err(e) => why.push(format!("{e:#}")),
        }
    }
    let (logged_at, log_index, log) = logged.ok_or_else(|| {
        let why =
            if why.is_empty() { "it has none (a Sigstore without one, as GitHub's own for private repositories, timestamps instead: kiln doesn't verify those)".to_owned() } else { why.join("; ") };
        anyhow::anyhow!("no transparency log entry of the bundle verifies: {why}")
    })?;

    // The certificate: a trusted Fulcio's, valid then, and seen by a CT log.
    let (_, leaf) = X509Certificate::from_der(&leaf_der).map_err(|e| anyhow::anyhow!("the bundle's certificate: {e}"))?;
    let issuer_spki = chain(&leaf, root, logged_at)?;
    sct(&leaf, &issuer_spki, root)?;

    // The envelope's signature: the certificate's key's.
    let key = Key::from_spki(leaf.public_key())?;
    anyhow::ensure!(key.verifies(key.natural(), &kiln_ci::attest::pae(payload_type, &payload), &signature), "the envelope's signature isn't the certificate's key's");

    // Who signed.
    let (issuer, subject, claims) = identity_of(&leaf)?;
    anyhow::ensure!(issuer == identity.issuer, "signed by an identity of {issuer}, not of {}", identity.issuer);
    match &identity.subject {
        Some(Subject::Exact(s)) => anyhow::ensure!(subject == *s, "signed by {subject}, not {s}"),
        Some(Subject::Prefix(p)) => anyhow::ensure!(subject.starts_with(p.as_str()), "signed by {subject}, not by {p}…"),
        None => {}
    }
    for (name, want) in &identity.claims {
        let got = claims.get(name).map(String::as_str).unwrap_or("not said");
        anyhow::ensure!(got == want, "its {name} is {got}, not {want}");
    }

    // What it says, and of what.
    anyhow::ensure!(payload_type == kiln_ci::attest::PAYLOAD_TYPE, "the envelope carries {payload_type}, not an in-toto statement");
    let statement: Value = serde_json::from_slice(&payload).map_err(|e| anyhow::anyhow!("the statement isn't JSON: {e}"))?;
    anyhow::ensure!(statement.get("_type").and_then(Value::as_str).is_some_and(|t| t.starts_with("https://in-toto.io/Statement/")), "the payload isn't an in-toto statement");
    if let Some((alg, hex)) = digest {
        let names_it = statement
            .get("subject")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|s| s.get("digest").and_then(|d| d.get(alg)).and_then(Value::as_str).is_some_and(|h| h.eq_ignore_ascii_case(hex)));
        anyhow::ensure!(names_it, "the statement isn't about {alg}:{hex}");
    }
    Ok(Verified { statement, issuer, subject, claims, logged_at, log_index, log })
}

/// A Rekor (v1) entry of the envelope: its body is of this payload, signature and certificate;
/// the log's promise (its signed entry timestamp) verifies, and its inclusion proof, needed when
/// `proof` says so, leads to a checkpoint the log signed. When the log took it, the entry's index,
/// and the log's URL.
fn logged_entry(e: &Value, root: &TrustedRoot, proof: bool, payload: &[u8], signature: &[u8], leaf: &[u8]) -> anyhow::Result<(i64, i64, String)> {
    let number = |v: Option<&Value>| v.and_then(|x| x.as_str().and_then(|s| s.parse::<i64>().ok()).or_else(|| x.as_i64()));
    let log_index = number(e.get("logIndex")).ok_or_else(|| anyhow::anyhow!("an entry with no index"))?;
    let at = number(e.get("integratedTime")).ok_or_else(|| anyhow::anyhow!("an entry with no time"))?;
    let id = B64.decode(e.pointer("/logId/keyId").and_then(Value::as_str).unwrap_or_default())?;
    let log =
        root.tlogs.iter().find(|l| l.id == id && l.valid.holds(at)).ok_or_else(|| anyhow::anyhow!("entry {log_index} is in log {}, which the trusted root doesn't trust then", hex::encode(&id)))?;
    let kind = e.pointer("/kindVersion/kind").and_then(Value::as_str).unwrap_or_default();
    let version = e.pointer("/kindVersion/version").and_then(Value::as_str).unwrap_or_default();
    let body_b64 = e.get("canonicalizedBody").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("entry {log_index} has no body"))?;
    let body = B64.decode(body_b64)?;
    let b: Value = serde_json::from_slice(&body).map_err(|e| anyhow::anyhow!("entry {log_index}'s body isn't JSON: {e}"))?;
    anyhow::ensure!(b.get("kind").and_then(Value::as_str) == Some(kind) && b.get("apiVersion").and_then(Value::as_str) == Some(version), "entry {log_index}'s body is of another kind than it says");
    match (kind, version) {
        ("dsse", "0.0.1") => dsse_body(&b, payload, signature, leaf)?,
        ("intoto", "0.0.2") => intoto_body(&b, payload, signature, leaf)?,
        (k, v) => anyhow::bail!("entry {log_index} is a {k} {v} entry: kiln reads Rekor's dsse 0.0.1 and intoto 0.0.2"),
    }
    // The log's promise: the body, its time and index, signed (canonical JSON, keys in order).
    let set = e
        .pointer("/inclusionPromise/signedEntryTimestamp")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("entry {log_index} has no signed entry timestamp, so when the log took it isn't signed"))?;
    let promised = format!("{{\"body\":{},\"integratedTime\":{at},\"logID\":\"{}\",\"logIndex\":{log_index}}}", serde_json::to_string(body_b64)?, hex::encode(&id));
    anyhow::ensure!(log.key.verifies(log.key.natural(), promised.as_bytes(), &B64.decode(set)?), "entry {log_index}'s signed entry timestamp isn't the log's");
    match e.get("inclusionProof") {
        Some(p) => inclusion(p, &body, log).map_err(|e| e.context(format!("entry {log_index}")))?,
        None => anyhow::ensure!(!proof, "entry {log_index} has no inclusion proof"),
    }
    Ok((at, log_index, log.url.clone()))
}

/// The certificate's DER in a PEM `verifier` (base64 of the PEM, as Rekor's bodies have it).
fn verifier_der(b64_pem: &str) -> Option<Vec<u8>> {
    let pem = String::from_utf8(B64.decode(b64_pem).ok()?).ok()?;
    super::signing::der_of(&pem).filter(|(label, _)| label == "CERTIFICATE").map(|(_, der)| der)
}

/// A `dsse` 0.0.1 body: the payload's hash, and the signature with its certificate.
fn dsse_body(b: &Value, payload: &[u8], signature: &[u8], leaf: &[u8]) -> anyhow::Result<()> {
    let s = |p: &str| b.pointer(p).and_then(Value::as_str).unwrap_or_default();
    anyhow::ensure!(s("/spec/payloadHash/algorithm") == "sha256" && s("/spec/payloadHash/value").eq_ignore_ascii_case(&hex::encode(sha256(payload))), "the log's entry is of another payload");
    let signed = b.pointer("/spec/signatures").and_then(Value::as_array).into_iter().flatten().any(|x| {
        x.get("signature").and_then(Value::as_str).and_then(|v| B64.decode(v).ok()).as_deref() == Some(signature)
            && x.get("verifier").and_then(Value::as_str).and_then(verifier_der).as_deref() == Some(leaf)
    });
    anyhow::ensure!(signed, "the log's entry is of another signature or certificate");
    Ok(())
}

/// An `intoto` 0.0.2 body: the payload's hash, and the signature (base64 of its base64) with its
/// certificate.
fn intoto_body(b: &Value, payload: &[u8], signature: &[u8], leaf: &[u8]) -> anyhow::Result<()> {
    let s = |p: &str| b.pointer(p).and_then(Value::as_str).unwrap_or_default();
    anyhow::ensure!(
        s("/spec/content/payloadHash/algorithm") == "sha256" && s("/spec/content/payloadHash/value").eq_ignore_ascii_case(&hex::encode(sha256(payload))),
        "the log's entry is of another payload"
    );
    let signed = b.pointer("/spec/content/envelope/signatures").and_then(Value::as_array).into_iter().flatten().any(|x| {
        let once = x.get("sig").and_then(Value::as_str).and_then(|v| B64.decode(v).ok()).unwrap_or_default();
        let twice = std::str::from_utf8(&once).ok().and_then(|t| B64.decode(t.trim()).ok()).unwrap_or_default();
        (once == signature || twice == signature) && x.get("publicKey").and_then(Value::as_str).and_then(verifier_der).as_deref() == Some(leaf)
    });
    anyhow::ensure!(signed, "the log's entry is of another signature or certificate");
    Ok(())
}

fn node(left: &[u8], right: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(65);
    b.push(1);
    b.extend_from_slice(left);
    b.extend_from_slice(right);
    sha256(&b)
}

/// The root an RFC 6962 inclusion proof of `leaf` (its leaf hash) at `index` in a tree of `size`
/// leads to.
fn merkle_root(index: u64, size: u64, leaf: &[u8], proof: &[Vec<u8>]) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(index < size, "the proof's index {index} is past its tree's {size} entries");
    // The hashes below the point where the leaf's path and the tree's right edge part, then
    // those along the edge.
    let inner = (u64::BITS - (index ^ (size - 1)).leading_zeros()) as usize;
    let border = index.checked_shr(u32::try_from(inner)?).unwrap_or(0).count_ones() as usize;
    anyhow::ensure!(proof.len() == inner + border, "the proof has {} hashes, not {}", proof.len(), inner + border);
    let mut h = leaf.to_vec();
    for (i, x) in proof.iter().enumerate() {
        let left = i < inner && index.checked_shr(u32::try_from(i)?).unwrap_or(0) & 1 == 0;
        h = if left { node(&h, x) } else { node(x, &h) };
    }
    Ok(h)
}

/// The inclusion proof of entry `body` in `log`: it leads to its root hash, and a checkpoint the
/// log signed names that root for that tree.
fn inclusion(p: &Value, body: &[u8], log: &Log) -> anyhow::Result<()> {
    let number = |k: &str| p.get(k).and_then(|x| x.as_str().and_then(|s| s.parse::<u64>().ok()).or_else(|| x.as_u64()));
    let (index, size) =
        (number("logIndex").ok_or_else(|| anyhow::anyhow!("an inclusion proof with no index"))?, number("treeSize").ok_or_else(|| anyhow::anyhow!("an inclusion proof with no tree size"))?);
    let root_hash = B64.decode(p.get("rootHash").and_then(Value::as_str).unwrap_or_default())?;
    let hashes = p.get("hashes").and_then(Value::as_array).into_iter().flatten().map(|h| Ok(B64.decode(h.as_str().unwrap_or_default())?)).collect::<anyhow::Result<Vec<Vec<u8>>>>()?;
    let mut leaf = vec![0u8];
    leaf.extend_from_slice(body);
    anyhow::ensure!(merkle_root(index, size, &sha256(&leaf), &hashes)? == root_hash, "its inclusion proof doesn't lead to its root hash");
    let note = p.pointer("/checkpoint/envelope").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("its inclusion proof has no checkpoint"))?;
    let (cp_size, cp_root) = checkpoint(note, log)?;
    anyhow::ensure!(cp_size == size && cp_root == root_hash, "its checkpoint is of another tree than its proof");
    Ok(())
}

/// A checkpoint (a signed note: origin, tree size, root hash, then the signatures): its tree size
/// and root hash, when the log's key signed it.
fn checkpoint(note: &str, log: &Log) -> anyhow::Result<(u64, Vec<u8>)> {
    let (text, signatures) = note.split_once("\n\n").ok_or_else(|| anyhow::anyhow!("its checkpoint isn't a signed note"))?;
    let signed = format!("{text}\n");
    let mut lines = text.lines().skip(1);
    let size = lines.next().and_then(|l| l.parse::<u64>().ok()).ok_or_else(|| anyhow::anyhow!("its checkpoint has no tree size"))?;
    let root = B64.decode(lines.next().unwrap_or_default())?;
    // Each signature line: an em dash, the signer's name, and base64 of a 4-byte key hint and
    // the signature.
    let by_log = signatures
        .lines()
        .filter_map(|l| l.strip_prefix("\u{2014} "))
        .filter_map(|l| l.rsplit_once(' '))
        .filter_map(|(_, s)| B64.decode(s).ok())
        .any(|raw| raw.get(4..).is_some_and(|s| log.key.verifies(log.key.natural(), signed.as_bytes(), s)));
    anyhow::ensure!(by_log, "its checkpoint isn't signed by the log");
    Ok((size, root))
}

fn valid_at(c: &X509Certificate<'_>, t: i64) -> bool {
    c.validity().not_before.timestamp() <= t && t <= c.validity().not_after.timestamp()
}

/// Whether `issuer`'s key made `cert`'s signature.
fn signed_by(cert: &X509Certificate<'_>, issuer: &X509Certificate<'_>) -> anyhow::Result<()> {
    let key = Key::from_spki(issuer.public_key())?;
    let alg = Alg::of_oid(&cert.signature_algorithm.algorithm.to_id_string())?;
    anyhow::ensure!(key.verifies(alg, cert.tbs_certificate.as_ref(), &cert.signature_value.data), "{}'s certificate isn't signed by {}", cert.subject(), issuer.subject());
    Ok(())
}

/// The certificate's chain to a Fulcio the trusted root trusts at `t`, every certificate in it
/// valid then: the issuing CA's SubjectPublicKeyInfo (for the SCT).
fn chain(leaf: &X509Certificate<'_>, root: &TrustedRoot, t: i64) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(valid_at(leaf, t), "the certificate wasn't valid when the log took the signature");
    anyhow::ensure!(!leaf.is_ca(), "the certificate is a CA's");
    anyhow::ensure!(leaf.key_usage()?.is_some_and(|k| k.value.digital_signature()), "the certificate isn't for signatures (its key usage)");
    anyhow::ensure!(leaf.extended_key_usage()?.is_some_and(|k| k.value.code_signing), "the certificate isn't for code signing (its extended key usage)");
    let mut why = Vec::new();
    for ca in root.cas.iter().filter(|c| c.valid.holds(t)) {
        let to = || -> anyhow::Result<Vec<u8>> {
            let certs = ca.certs.iter().map(|d| X509Certificate::from_der(d).map(|(_, c)| c).map_err(|e| anyhow::anyhow!("{}'s chain: {e}", ca.uri))).collect::<anyhow::Result<Vec<_>>>()?;
            let first = certs.first().ok_or_else(|| anyhow::anyhow!("{} has no certificates", ca.uri))?;
            anyhow::ensure!(leaf.issuer().as_raw() == first.subject().as_raw(), "issued by {}, not {}", leaf.issuer(), first.subject());
            signed_by(leaf, first)?;
            for (n, c) in certs.iter().enumerate() {
                anyhow::ensure!(c.is_ca() && valid_at(c, t), "{} wasn't a CA then", c.subject());
                // Each by the next; the root by itself.
                signed_by(c, certs.get(n + 1).unwrap_or(c))?;
            }
            Ok(first.public_key().raw.to_vec())
        };
        match to() {
            Ok(spki) => return Ok(spki),
            Err(e) => why.push(format!("{e:#}")),
        }
    }
    anyhow::bail!("the certificate isn't one a Fulcio of the trusted root issued then: {}", if why.is_empty() { "none was trusted then".to_owned() } else { why.join("; ") })
}

/// One DER element of `b`: its tag, its content, and what follows it.
fn der(b: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = b.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let n = usize::from(first & 0x7F);
        if n == 0 || n > 4 {
            return None;
        }
        let (lb, rest) = rest.split_at_checked(n)?;
        (lb.iter().fold(0usize, |a, &x| (a << 8) | usize::from(x)), rest)
    };
    let (content, after) = rest.split_at_checked(len)?;
    Some((tag, content, after))
}

/// A DER element of `tag` around `content`.
fn encode(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let n = content.len();
    if let Ok(short) = u8::try_from(n)
        && short < 0x80
    {
        out.push(short);
    } else {
        let bytes = n.to_be_bytes();
        let lb = bytes.get(bytes.iter().take_while(|&&b| b == 0).count()..).unwrap_or_default();
        out.push(0x80 | u8::try_from(lb.len()).unwrap_or_default());
        out.extend_from_slice(lb);
    }
    out.extend_from_slice(content);
    out
}

/// The elements of a DER sequence's content, whole.
fn elements(mut b: &[u8]) -> Option<Vec<(u8, &[u8], &[u8])>> {
    let mut out = Vec::new();
    while !b.is_empty() {
        let (tag, content, after) = der(b)?;
        out.push((tag, content, b.get(..b.len() - after.len())?));
        b = after;
    }
    Some(out)
}

/// The TBS certificate as a CT log signed it: the certificate's, without its SCT list (RFC 6962
/// §3.2's precertificate).
fn precert_tbs(tbs: &[u8]) -> anyhow::Result<Vec<u8>> {
    let bad = || anyhow::anyhow!("the certificate's DER");
    let (tag, content, _) = der(tbs).ok_or_else(bad)?;
    anyhow::ensure!(tag == 0x30, bad());
    let mut out = Vec::new();
    for (tag, content, whole) in elements(content).ok_or_else(bad)? {
        if tag != 0xA3 {
            out.extend_from_slice(whole);
            continue;
        }
        let (_, exts, _) = der(content).ok_or_else(bad)?;
        let kept: Vec<u8> = elements(exts).ok_or_else(bad)?.into_iter().filter(|(_, ext, _)| !ext.starts_with(SCT_LIST_OID)).flat_map(|(_, _, whole)| whole.to_vec()).collect();
        out.extend(encode(0xA3, &encode(0x30, &kept)));
    }
    Ok(encode(0x30, &out))
}

/// One of the certificate's SCTs is a CT log's of the trusted root: a log saw the certificate.
/// A trusted root with no CT logs (a Sigstore without one) asks for none.
fn sct(leaf: &X509Certificate<'_>, issuer_spki: &[u8], root: &TrustedRoot) -> anyhow::Result<()> {
    if root.ctlogs.is_empty() {
        return Ok(());
    }
    let scts = leaf
        .extensions()
        .iter()
        .find_map(|e| match e.parsed_extension() {
            ParsedExtension::SCT(l) => Some(l),
            _ => None,
        })
        .ok_or_else(|| anyhow::anyhow!("the certificate has no SCT: no CT log saw it"))?;
    let tbs = precert_tbs(leaf.tbs_certificate.as_ref())?;
    let tbs_len = u32::try_from(tbs.len())?.to_be_bytes();
    anyhow::ensure!(tbs_len.first() == Some(&0), "the certificate is too big for an SCT");
    let key_hash = sha256(issuer_spki);
    for s in scts {
        let Some(log) = root.ctlogs.iter().find(|l| l.id.as_slice() == s.id.key_id.as_slice() && l.valid.holds(i64::try_from(s.timestamp / 1000).unwrap_or(i64::MAX))) else { continue };
        let alg = match (s.signature.hash_alg_id, s.signature.sign_alg_id) {
            (4, 3) => Alg::Ecdsa(Hash::Sha256),
            (5, 3) => Alg::Ecdsa(Hash::Sha384),
            (4, 1) => Alg::RsaPkcs1(Hash::Sha256),
            _ => continue,
        };
        // RFC 6962 §3.2: v1, a certificate timestamp, of a precertificate entry.
        let mut signed = vec![0u8, 0u8];
        signed.extend_from_slice(&s.timestamp.to_be_bytes());
        signed.extend_from_slice(&[0, 1]);
        signed.extend_from_slice(&key_hash);
        signed.extend_from_slice(tbs_len.get(1..).unwrap_or_default());
        signed.extend_from_slice(&tbs);
        signed.extend_from_slice(&u16::try_from(s.extensions.0.len())?.to_be_bytes());
        signed.extend_from_slice(s.extensions.0);
        if log.key.verifies(alg, &signed, s.signature.data) {
            return Ok(());
        }
    }
    anyhow::bail!("no SCT of the certificate is a CT log's of the trusted root")
}

/// A DER UTF8String's text (Fulcio's later extensions are).
fn utf8_of(v: &[u8]) -> Option<String> {
    let (tag, content, rest) = der(v)?;
    if tag != 0x0C || !rest.is_empty() {
        return None;
    }
    String::from_utf8(content.to_vec()).ok()
}

/// The certificate's OIDC issuer, subject (its SAN: a URI or an email) and Fulcio's claims.
fn identity_of(leaf: &X509Certificate<'_>) -> anyhow::Result<(String, String, BTreeMap<&'static str, String>)> {
    let (mut v1, mut v2, mut claims) = (None, None, BTreeMap::new());
    for e in leaf.extensions() {
        let oid = e.oid.to_id_string();
        if oid == ISSUER_V2 {
            v2 = utf8_of(e.value);
        } else if oid == ISSUER_V1 {
            v1 = std::str::from_utf8(e.value).ok().map(str::to_owned);
        } else if let Some((_, name)) = CLAIMS.iter().find(|(o, _)| *o == oid)
            && let Some(v) = utf8_of(e.value)
        {
            claims.insert(*name, v);
        }
    }
    let issuer = v2.or(v1).ok_or_else(|| anyhow::anyhow!("the certificate names no OIDC issuer: not Fulcio's"))?;
    let subject = leaf
        .subject_alternative_name()?
        .and_then(|san| {
            san.value.general_names.iter().find_map(|g| match g {
                GeneralName::URI(u) => Some((*u).to_owned()),
                GeneralName::RFC822Name(m) => Some((*m).to_owned()),
                _ => None,
            })
        })
        .ok_or_else(|| anyhow::anyhow!("the certificate names no subject"))?;
    Ok((issuer, subject, claims))
}

// ------------------------------------------------------------------ GitHub's attestations

/// A raw snappy block, decompressed: GitHub keeps attestations' bundles so (`bundle_url`). At
/// most `max` bytes.
fn unsnappy(b: &[u8], max: usize) -> anyhow::Result<Vec<u8>> {
    let bad = || anyhow::anyhow!("not a snappy block");
    let (mut want, mut shift, mut rest) = (0usize, 0u32, b);
    loop {
        let (&c, r) = rest.split_first().ok_or_else(bad)?;
        rest = r;
        want |= usize::from(c & 0x7F).checked_shl(shift).ok_or_else(bad)?;
        if c < 0x80 {
            break;
        }
        shift += 7;
        anyhow::ensure!(shift < 35, bad());
    }
    anyhow::ensure!(want <= max, "a compressed bundle of {want} bytes: more than {max}");
    let le = |b: &[u8]| b.iter().rev().fold(0usize, |a, &x| (a << 8) | usize::from(x));
    let mut out = Vec::with_capacity(want);
    while let Some((&tag, r)) = rest.split_first() {
        rest = r;
        let (len, offset) = match tag & 3 {
            0 => {
                let mut len = usize::from(tag >> 2);
                if len >= 60 {
                    let (lb, r) = rest.split_at_checked(len - 59).ok_or_else(bad)?;
                    rest = r;
                    len = le(lb);
                }
                let (lit, r) = rest.split_at_checked(len + 1).ok_or_else(bad)?;
                rest = r;
                out.extend_from_slice(lit);
                anyhow::ensure!(out.len() <= want, bad());
                continue;
            }
            1 => {
                let (&x, r) = rest.split_first().ok_or_else(bad)?;
                rest = r;
                (usize::from((tag >> 2) & 7) + 4, (usize::from(tag >> 5) << 8) | usize::from(x))
            }
            n => {
                let (o, r) = rest.split_at_checked(if n == 2 { 2 } else { 4 }).ok_or_else(bad)?;
                rest = r;
                (usize::from(tag >> 2) + 1, le(o))
            }
        };
        anyhow::ensure!(offset > 0 && offset <= out.len() && out.len() + len <= want, bad());
        for _ in 0..len {
            let x = out.get(out.len() - offset).copied().ok_or_else(bad)?;
            out.push(x);
        }
    }
    anyhow::ensure!(out.len() == want, bad());
    Ok(out)
}

/// The bundles GitHub's attestations API (`api`) has of `digest` (`sha256:<hex>`) in repository
/// `repo`: each inline, or behind its `bundle_url`, snappy-compressed.
pub async fn github_bundles(api: &str, repo: &str, digest: &str, token: Option<&str>) -> anyhow::Result<Vec<Value>> {
    let http = crate::http::client::github()?;
    let url = format!("{}/repos/{}/attestations/{digest}?per_page=100", api.trim_end_matches('/'), repo.trim_matches('/'));
    let mut req = http.get(&url).header("Accept", "application/vnd.github+json").header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(t) = token {
        req = req.bearer(t);
    }
    let r = req.send().await.map_err(|e| anyhow::anyhow!("{url}: {e:#}"))?;
    let status = r.status();
    let body = r.bytes(MAX_BUNDLE).await?;
    anyhow::ensure!(status != 404, "GitHub has no attestations of {digest} in {repo} (or can't show them without a token: GH_TOKEN)");
    anyhow::ensure!((200..300).contains(&status), "{url}: {status} {}", String::from_utf8_lossy(body.get(..body.len().min(300)).unwrap_or_default()));
    let v: Value = serde_json::from_slice(&body)?;
    let mut out = Vec::new();
    for a in v.get("attestations").and_then(Value::as_array).into_iter().flatten() {
        if let Some(b) = a.get("bundle").filter(|b| b.is_object()) {
            out.push(b.clone());
            continue;
        }
        let Some(url) = a.get("bundle_url").and_then(Value::as_str) else { continue };
        // A signed URL of GitHub's store: no token goes with it.
        let r = http.get(url).send().await.map_err(|e| anyhow::anyhow!("an attestation's bundle: {e:#}"))?;
        anyhow::ensure!(r.is_success(), "an attestation's bundle: {}", r.status());
        let snappy = r.header("content-type").is_some_and(|t| t.contains("snappy")) || url.split('?').next().is_some_and(|p| p.ends_with(".sn"));
        let raw = r.bytes(MAX_BUNDLE).await?;
        let json = if snappy { unsnappy(&raw, MAX_BUNDLE)? } else { raw.to_vec() };
        out.push(serde_json::from_slice(&json).map_err(|e| anyhow::anyhow!("an attestation's bundle isn't JSON: {e}"))?);
    }
    Ok(out)
}

/// The trusted root keyless bundles are verified against: `[ci.attest] trusted_root`, else
/// Sigstore's public instance's (kept under the data directory).
pub async fn trusted_root(cfg: &crate::config::Config) -> anyhow::Result<TrustedRoot> {
    match &cfg.ci.attest.trusted_root {
        Some(p) => TrustedRoot::from_json(&std::fs::read(p).map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))?),
        None => tuf::public_good(&cfg.server.data_dir.join("sigstore")).await,
    }
}

// ------------------------------------------------------------------ the CLI

#[derive(clap::Args, Debug)]
pub struct AttestArgs {
    #[command(subcommand)]
    pub cmd: AttestCmd,
}

#[derive(clap::Subcommand, Debug)]
pub enum AttestCmd {
    /// Whether an artifact's attestations verify: Sigstore bundles signed keyless, as GitHub's
    /// `actions/attest-build-provenance` and kiln's keyless signing make them.
    Verify(VerifyArgs),
}

#[derive(clap::Args, Debug)]
pub struct VerifyArgs {
    /// The artifact: a file, or its digest (`sha256:<hex>`).
    pub artifact: String,
    /// The bundle (a `.sigstore.json`, or JSON Lines of them); without one, the attestations
    /// GitHub has of the artifact in `--repo`.
    #[arg(long)]
    pub bundle: Option<PathBuf>,
    /// The GitHub repository (`owner/name`) whose workflow must have built it from its source.
    #[arg(long)]
    pub repo: Option<String>,
    /// The workflow that must have signed it: `.github/workflows/release.yml` of `--repo`, or
    /// `owner/name/.github/workflows/…` (a reusable one).
    #[arg(long)]
    pub signer_workflow: Option<String>,
    /// The ref its source must have been at (`refs/heads/main`).
    #[arg(long)]
    pub source_ref: Option<String>,
    /// Another OIDC issuer than GitHub Actions' (kiln's own, for its keyless signing).
    #[arg(long)]
    pub issuer: Option<String>,
    /// The certificate's subject, exactly (with `--issuer`).
    #[arg(long)]
    pub subject: Option<String>,
    /// A Sigstore trusted root (`trusted_root.json`); without one, Sigstore's public instance's,
    /// through its TUF repository.
    #[arg(long)]
    pub trusted_root: Option<PathBuf>,
    /// GitHub's API (GitHub Enterprise Server's: `https://HOST/api/v3`). `GH_TOKEN` or
    /// `GITHUB_TOKEN`, when set, goes to it, for private repositories.
    #[arg(long, default_value = "https://api.github.com")]
    pub api: String,
    /// What verified, as JSON.
    #[arg(long)]
    pub json: bool,
}

/// `kiln attest verify`: an error unless one of the artifact's bundles verifies.
pub async fn cli(a: AttestArgs, cfg: &crate::config::Config) -> anyhow::Result<()> {
    let AttestCmd::Verify(v) = a.cmd;
    let hex = match v.artifact.strip_prefix("sha256:") {
        Some(h) => {
            anyhow::ensure!(h.len() == 64 && h.bytes().all(|c| c.is_ascii_hexdigit()), "`{}`: sha256:<64 hex digits>", v.artifact);
            h.to_ascii_lowercase()
        }
        None => {
            let path = PathBuf::from(&v.artifact);
            crate::rt::blocking(move || -> anyhow::Result<String> {
                use sha2::Digest as _;
                use std::io::Read as _;
                let mut f = std::fs::File::open(&path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
                let (mut h, mut buf) = (sha2::Sha256::new(), vec![0u8; 1 << 20]);
                loop {
                    let n = f.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    h.update(buf.get(..n).unwrap_or_default());
                }
                Ok(hex::encode(h.finalize()))
            })
            .await??
        }
    };
    let identity = match (&v.issuer, &v.repo) {
        (Some(issuer), _) => Identity { issuer: issuer.clone(), subject: v.subject.clone().map(Subject::Exact), claims: Vec::new() },
        (None, Some(repo)) => Identity::github(repo, v.signer_workflow.as_deref(), v.source_ref.as_deref()),
        (None, None) => anyhow::bail!("say who must have signed it: --repo OWNER/NAME (a GitHub workflow), or --issuer"),
    };
    let root = match &v.trusted_root {
        Some(p) => TrustedRoot::from_json(&std::fs::read(p).map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))?)?,
        None => trusted_root(cfg).await?,
    };
    let bundles: Vec<Value> = match &v.bundle {
        Some(p) => {
            let text = std::fs::read_to_string(p).map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))?;
            match serde_json::from_str::<Value>(&text) {
                Ok(b) => vec![b],
                Err(_) => text
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(serde_json::from_str)
                    .collect::<Result<_, _>>()
                    .map_err(|e| anyhow::anyhow!("{}: neither a bundle nor JSON Lines of them: {e}", p.display()))?,
            }
        }
        None => {
            let repo = v.repo.as_deref().ok_or_else(|| anyhow::anyhow!("--bundle FILE, or --repo OWNER/NAME to read GitHub's"))?;
            let token = std::env::var("GH_TOKEN").or_else(|_| std::env::var("GITHUB_TOKEN")).ok().filter(|t| !t.is_empty());
            github_bundles(&v.api, repo, &format!("sha256:{hex}"), token.as_deref()).await?
        }
    };
    anyhow::ensure!(!bundles.is_empty(), "no attestations of sha256:{hex}");
    let results: Vec<anyhow::Result<Verified>> = bundles.iter().map(|b| verify(b, &root, &identity, Some(("sha256", hex.as_str())))).collect();
    let good = results.iter().filter(|r| r.is_ok()).count();
    if v.json {
        let list: Vec<Value> = results
            .iter()
            .map(|r| match r {
                Ok(x) => json!({ "verified": true, "attestation": x.to_json() }),
                Err(e) => json!({ "verified": false, "why": format!("{e:#}") }),
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json!({ "digest": format!("sha256:{hex}"), "verified": good, "attestations": list }))?);
    } else {
        println!("sha256:{hex}: {good} of {} attestations verified", results.len());
        for r in &results {
            match r {
                Ok(x) => {
                    let at = ::time::OffsetDateTime::from_unix_timestamp(x.logged_at).ok().and_then(|t| t.format(&::time::format_description::well_known::Rfc3339).ok()).unwrap_or_default();
                    let source =
                        x.claims.get("source_repository_uri").map(|s| format!(", from {s}@{}", x.claims.get("source_repository_digest").map(String::as_str).unwrap_or("?"))).unwrap_or_default();
                    println!("  verified: {} signed by {}{source}; logged {at}, {} entry {}", x.predicate_type(), x.subject, x.log, x.log_index);
                }
                Err(e) => println!("  not verified: {e:#}"),
            }
        }
    }
    anyhow::ensure!(good > 0, "no attestation of sha256:{hex} verified");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The GitHub CLI's provenance of its 2.103.2 release (`actions/attest-build-provenance` in
    /// cli/cli's deployment workflow, on Sigstore's public instance), as GitHub's attestations API
    /// gave it, and Sigstore's trusted root then.
    const BUNDLE: &[u8] = include_bytes!("testdata/gh-provenance.sigstore.json");
    const TRUSTED_ROOT: &[u8] = include_bytes!("testdata/trusted_root.json");
    /// GitHub's own attestation of the release (its own Sigstore: timestamps, no log), compressed
    /// as its API keeps it.
    const RELEASE: &[u8] = include_bytes!("testdata/gh-release.sigstore.json");
    const RELEASE_SN: &[u8] = include_bytes!("testdata/gh-release.sigstore.json.sn");
    const TARBALL: &str = "62ccf747115ddc61237efffdde66a3c8f70ba249433dc64f70801089b59c59c8";

    fn fixture() -> anyhow::Result<(Value, TrustedRoot)> {
        Ok((serde_json::from_slice(BUNDLE)?, TrustedRoot::from_json(TRUSTED_ROOT)?))
    }

    fn cli_cli() -> Identity {
        Identity::github("cli/cli", Some(".github/workflows/deployment.yml"), Some("refs/heads/trunk"))
    }

    /// GitHub's attestation verifies: its chain, SCT, signature, log entry (promise, proof and
    /// checkpoint) and identity, about the tarball.
    #[test]
    fn w18_github_attestation_verifies() -> anyhow::Result<()> {
        let (b, root) = fixture()?;
        let v = verify(&b, &root, &cli_cli(), Some(("sha256", TARBALL)))?;
        assert_eq!(v.predicate_type(), "https://slsa.dev/provenance/v1");
        assert_eq!(v.issuer, GITHUB_ISSUER);
        assert_eq!(v.subject, "https://github.com/cli/cli/.github/workflows/deployment.yml@refs/heads/trunk");
        assert_eq!(v.claims.get("source_repository_digest").map(String::as_str), Some("4f2cfe92e3499f8581bf39db77e0a1431fa7b55c"));
        assert_eq!(v.claims.get("runner_environment").map(String::as_str), Some("github-hosted"));
        assert_eq!((v.logged_at, v.log_index, v.log.as_str()), (1_792_786_936, 3_425_010_592, "https://rekor.sigstore.dev"));
        // Any workflow of the repository, from any ref, and the identity's subject alone.
        verify(&b, &root, &Identity::github("cli/cli", None, None), None)?;
        verify(&b, &root, &Identity { issuer: GITHUB_ISSUER.into(), subject: Some(Subject::Exact(v.subject.clone())), claims: Vec::new() }, None)?;
        Ok(())
    }

    /// Doesn't verify, saying `says` (any reason, when empty).
    fn fails(b: &Value, root: &TrustedRoot, identity: &Identity, digest: Option<(&str, &str)>, says: &str) -> anyhow::Result<()> {
        match verify(b, root, identity, digest) {
            Ok(_) => anyhow::bail!("verified, though it shouldn't have ({says})"),
            Err(e) => anyhow::ensure!(format!("{e:#}").contains(says), "{e:#} (wanted: {says})"),
        }
        Ok(())
    }

    fn set(b: &Value, pointer: &str, to: Value) -> Value {
        let mut b = b.clone();
        if let Some(x) = b.pointer_mut(pointer) {
            *x = to;
        }
        b
    }

    /// A b64 string with one byte of what it encodes changed.
    fn flipped(b: &Value, pointer: &str, at: usize) -> Value {
        let mut raw = B64.decode(b.pointer(pointer).and_then(Value::as_str).unwrap_or_default()).unwrap_or_default();
        if let Some(x) = raw.get_mut(at) {
            *x ^= 1;
        }
        set(b, pointer, Value::String(B64.encode(raw)))
    }

    /// Another artifact, signer, repository, ref or issuer than the attestation's doesn't verify.
    #[test]
    fn w18_github_attestation_is_of_its_artifact_and_signer() -> anyhow::Result<()> {
        let (b, root) = fixture()?;
        fails(&b, &root, &cli_cli(), Some(("sha256", "0".repeat(64).as_str())), "isn't about sha256:0000")?;
        fails(&b, &root, &Identity::github("cli/cli", Some(".github/workflows/other.yml"), None), None, "not by https://github.com/cli/cli/.github/workflows/other.yml@")?;
        fails(&b, &root, &Identity::github("someone/cli", None, None), None, "not by https://github.com/someone/cli/")?;
        fails(&b, &root, &Identity::github("cli/cli", None, Some("refs/heads/main")), None, "its source_repository_ref is refs/heads/trunk, not refs/heads/main")?;
        fails(&b, &root, &Identity { issuer: "https://accounts.google.com".into(), subject: None, claims: Vec::new() }, None, "not of https://accounts.google.com")?;
        Ok(())
    }

    /// Each part of the bundle changed fails the check that covers it.
    #[test]
    fn w18_github_attestation_tampered() -> anyhow::Result<()> {
        let (b, root) = fixture()?;
        let id = cli_cli();
        let e = "/verificationMaterial/tlogEntries/0";
        // The statement, and the envelope's signature.
        fails(&flipped(&b, "/dsseEnvelope/payload", 40), &root, &id, None, "another payload")?;
        fails(&flipped(&b, "/dsseEnvelope/signatures/0/sig", 20), &root, &id, None, "another signature or certificate")?;
        // The log's promise, its time and index (which it signs), its proof and checkpoint.
        fails(&flipped(&b, &format!("{e}/inclusionPromise/signedEntryTimestamp"), 20), &root, &id, None, "signed entry timestamp isn't the log's")?;
        fails(&set(&b, &format!("{e}/integratedTime"), json!("1792786937")), &root, &id, None, "signed entry timestamp isn't the log's")?;
        fails(&flipped(&b, &format!("{e}/inclusionProof/hashes/3"), 5), &root, &id, None, "doesn't lead to its root hash")?;
        fails(&set(&b, &format!("{e}/inclusionProof/treeSize"), json!("3303106343")), &root, &id, None, "")?;
        let cp = b.pointer(&format!("{e}/inclusionProof/checkpoint/envelope")).and_then(Value::as_str).unwrap_or_default().replacen("\n3303106342\n", "\n3303106343\n", 1);
        fails(&set(&b, &format!("{e}/inclusionProof/checkpoint/envelope"), json!(cp)), &root, &id, None, "checkpoint isn't signed by the log")?;
        // No proof: a v0.3 bundle needs one.
        let mut no_proof = b.clone();
        if let Some(t) = no_proof.pointer_mut(e).and_then(Value::as_object_mut) {
            t.remove("inclusionProof");
        }
        fails(&no_proof, &root, &id, None, "has no inclusion proof")?;
        // The certificate: another one's bytes break its chain (and the log's entry names it).
        fails(&flipped(&b, "/verificationMaterial/certificate/rawBytes", 300), &root, &id, None, "")?;
        Ok(())
    }

    /// A trusted root without the bundle's Fulcio, CT log or Rekor doesn't verify it.
    #[test]
    fn w18_the_trusted_root_decides() -> anyhow::Result<()> {
        let (b, root) = fixture()?;
        let id = cli_cli();
        fails(&b, &TrustedRoot { cas: Vec::new(), ..root.clone() }, &id, None, "isn't one a Fulcio of the trusted root issued then")?;
        fails(&b, &TrustedRoot { tlogs: Vec::new(), ..root.clone() }, &id, None, "which the trusted root doesn't trust then")?;
        let other_ct = root.ctlogs.iter().filter(|l| !l.url.ends_with("/2022")).cloned().collect();
        fails(&b, &TrustedRoot { ctlogs: other_ct, ..root.clone() }, &id, None, "no SCT of the certificate is a CT log's")?;
        // Fulcio's CA trusted only until before it was signed.
        let ended = root.cas.iter().map(|c| Ca { valid: Window { start: c.valid.start, end: Some(1_792_786_000) }, ..c.clone() }).collect();
        fails(&b, &TrustedRoot { cas: ended, ..root.clone() }, &id, None, "none was trusted then")?;
        // A Sigstore without CT logs asks for no SCT.
        verify(&b, &TrustedRoot { ctlogs: Vec::new(), ..root }, &id, None)?;
        Ok(())
    }

    /// GitHub's own Sigstore (a private repository's, or its release attestations') timestamps
    /// instead of logging: kiln says so rather than passing it.
    #[test]
    fn w18_bundles_without_a_log_are_refused() -> anyhow::Result<()> {
        let (_, root) = fixture()?;
        let b: Value = serde_json::from_slice(RELEASE)?;
        fails(&b, &root, &Identity::github("cli/cli", None, None), None, "kiln doesn't verify those")?;
        Ok(())
    }

    /// GitHub's compressed bundles decompress to themselves; broken blocks are refused.
    #[test]
    fn w18_snappy() -> anyhow::Result<()> {
        assert_eq!(unsnappy(RELEASE_SN, MAX_BUNDLE)?, RELEASE);
        assert!(unsnappy(RELEASE_SN, 1000).is_err(), "bigger than allowed");
        assert!(unsnappy(RELEASE_SN.get(..RELEASE_SN.len() - 7).unwrap_or_default(), MAX_BUNDLE).is_err(), "cut short");
        // A copy from before the start.
        assert!(unsnappy(&[0x05, 0x01, 0x05], MAX_BUNDLE).is_err());
        assert_eq!(unsnappy(&[0x05, 0x10, b'h', b'e', b'l', b'l', b'o'], MAX_BUNDLE)?, b"hello");
        // "ab" and a copy of 6 from 2 back is "abababab"; one of 4 falls short of the 8 it says.
        assert_eq!(unsnappy(&[0x08, 0x04, b'a', b'b', 0x09, 0x02], MAX_BUNDLE)?, b"abababab");
        assert!(unsnappy(&[0x08, 0x04, b'a', b'b', 0x01, 0x02], MAX_BUNDLE).is_err());
        Ok(())
    }

    /// RFC 6962's proofs, for every leaf of trees of every size up to 9, built the long way.
    #[test]
    fn merkle_proofs() -> anyhow::Result<()> {
        fn leaf(i: u64) -> Vec<u8> {
            sha256(&[[0u8].as_slice(), i.to_be_bytes().as_slice()].concat())
        }
        // The tree's root and a leaf's path in it, as RFC 6962 §2.1 defines them.
        fn tree(leaves: &[Vec<u8>]) -> Vec<u8> {
            match leaves {
                [] => sha256(b""),
                [one] => one.clone(),
                _ => {
                    let k = leaves.len().next_power_of_two() / 2;
                    let (l, r) = leaves.split_at(k);
                    node(&tree(l), &tree(r))
                }
            }
        }
        fn path(m: usize, leaves: &[Vec<u8>]) -> Vec<Vec<u8>> {
            if leaves.len() <= 1 {
                return Vec::new();
            }
            let k = leaves.len().next_power_of_two() / 2;
            let (l, r) = leaves.split_at(k);
            if m < k { [path(m, l), vec![tree(r)]].concat() } else { [path(m - k, r), vec![tree(l)]].concat() }
        }
        for size in 1..=9u64 {
            let leaves: Vec<Vec<u8>> = (0..size).map(leaf).collect();
            let root = tree(&leaves);
            for i in 0..size {
                let p = path(usize::try_from(i)?, &leaves);
                assert_eq!(merkle_root(i, size, &leaf(i), &p)?, root, "leaf {i} of {size}");
                if let Some(first) = p.first() {
                    assert_ne!(merkle_root(i, size, first, &p)?, root, "another leaf than {i} of {size}");
                }
            }
            assert!(merkle_root(size, size, &leaf(0), &[]).is_err());
        }
        Ok(())
    }

    /// The precertificate's TBS drops the SCT list alone: a certificate without one comes back
    /// as it was.
    #[test]
    fn precert_tbs_drops_only_the_scts() -> anyhow::Result<()> {
        let (b, root) = fixture()?;
        let ca = root.cas.iter().find_map(|c| c.certs.first()).ok_or_else(|| anyhow::anyhow!("no CA"))?;
        let (_, c) = X509Certificate::from_der(ca).map_err(|e| anyhow::anyhow!("{e}"))?;
        assert_eq!(precert_tbs(c.tbs_certificate.as_ref())?, c.tbs_certificate.as_ref());
        let leaf = B64.decode(b.pointer("/verificationMaterial/certificate/rawBytes").and_then(Value::as_str).unwrap_or_default())?;
        let (_, l) = X509Certificate::from_der(&leaf).map_err(|e| anyhow::anyhow!("{e}"))?;
        let pre = precert_tbs(l.tbs_certificate.as_ref())?;
        assert!(pre.len() < l.tbs_certificate.as_ref().len());
        assert!(!pre.windows(SCT_LIST_OID.len()).any(|w| w == SCT_LIST_OID));
        Ok(())
    }
}
