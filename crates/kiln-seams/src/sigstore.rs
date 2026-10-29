//! `Sigstore`: an outside Fulcio and Rekor (docs/ci.md W18-R02, W27-R12). Fulcio certifies a key
//! for whoever an identity token names, the key's holder proving it has it; Rekor logs entries,
//! numbered in the order they come, promises each (its signed entry timestamp), proves each is in
//! its tree, and gives each back by its number. kiln's keyless signing goes through it. The real
//! one is kiln's client of a Fulcio and a Rekor it's configured with (`kiln::ci::signing::Remote`);
//! [`SimSigstore`] certifies with a CA of its own and logs in a Merkle tree of its own, its promises
//! and checkpoints signed with a key of its own, so kiln's Sigstore verifier verifies what it signs
//! against [`SimSigstore::trusted_root`]. Its keys come from a seed and its times count a second a
//! request from a start it's given, so a run is the same every time.

use std::collections::HashMap;
use std::sync::Arc;

use aws_lc_rs::signature::{self as sig, Ed25519KeyPair, KeyPair as _, UnparsedPublicKey, VerificationAlgorithm};
use base64::Engine as _;
use futures::future::BoxFuture;
use parking_lot::Mutex;
use serde_json::{Value, json};
use x509_parser::prelude::{FromDer as _, X509Certificate};
use x509_parser::x509::SubjectPublicKeyInfo;

use crate::conformance::{Case, Failure, ensure, fine, refused, same};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;
const B64URL: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// The most entries [`SimSigstore`] logs; past it, refused `Full`.
pub const SIM_ENTRIES: usize = 100_000;
/// How long a certificate [`SimSigstore`] issues is valid, in seconds (Fulcio's ten minutes).
pub const SIM_CERT_SECONDS: i64 = 600;
/// The issuer and subject the suite's identity tokens name.
pub const ISSUER: &str = "https://issuer.kiln-seams.test";
pub const SUBJECT: &str = "repo:acme/app:ref:refs/heads/main";
/// Fulcio's OIDC issuer extensions: 1.3.6.1.4.1.57264.1.8 (a DER UTF8String) and .1 (the bare string).
const ISSUER_V2: [u64; 9] = [1, 3, 6, 1, 4, 1, 57264, 1, 8];
const ISSUER_V1: [u64; 9] = [1, 3, 6, 1, 4, 1, 57264, 1, 1];
/// DSSE's payload type for in-toto statements.
const IN_TOTO: &str = "application/vnd.in-toto+json";
/// The simulated Rekor's name in its checkpoints.
const LOG_NAME: &str = "kiln-seams.sim/rekor";

/// A key to certify: the identity token naming who asks, the key (PEM), and the token's subject
/// signed with the key (the proof that the asker holds it).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CertifyAsk {
    pub token: String,
    pub public_key_pem: String,
    pub proof: Vec<u8>,
}

/// What Fulcio certified: its chain, PEM, the key's certificate first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Certified {
    pub chain: Vec<String>,
}

/// An entry in the log: its id, its number, when the log took it (unix seconds), its body as the
/// log keeps it (base64 of its canonical JSON), and the log's whole answer for it (its promise and
/// proof of inclusion).
#[derive(Debug, Clone, PartialEq)]
pub struct Logged {
    pub uuid: String,
    pub index: u64,
    pub integrated_time: i64,
    pub body: String,
    pub answer: Value,
}

/// Why a request was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("{what} isn't valid: {why}")]
    Invalid { what: &'static str, why: String },
    /// The identity token wasn't taken.
    #[error("the identity token was refused: {0}")]
    Unauthorized(String),
    /// The same entry is in the log already, at `index`.
    #[error("that entry is logged already, at {index}")]
    Conflict { index: u64 },
    #[error("no entry {index} in the log")]
    NotFound { index: u64 },
    #[error("the log holds its most ({limit})")]
    Full { limit: usize },
    #[error("sigstore is unavailable: {0}")]
    Unavailable(String),
}

/// An outside Fulcio and Rekor.
pub trait Sigstore: Send + Sync + 'static {
    /// Fulcio: a certificate chain for the key, naming whom the token names.
    fn certify(&self, ask: CertifyAsk) -> BoxFuture<'_, Result<Certified, Refusal>>;
    /// Rekor: logs a proposed entry (`{apiVersion, kind, spec}`).
    fn log(&self, entry: Value) -> BoxFuture<'_, Result<Logged, Refusal>>;
    /// Rekor: the entry at `index`.
    fn entry(&self, index: u64) -> BoxFuture<'_, Result<Logged, Refusal>>;
}

fn sha256(b: &[u8]) -> [u8; 32] {
    <[u8; 32]>::try_from(aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, b).as_ref()).unwrap_or([0; 32])
}

/// PEM of `der` under `label`, 64 columns.
pub fn pem(label: &str, der: &[u8]) -> String {
    let b64 = B64.encode(der);
    let lines: Vec<&str> = b64.as_bytes().chunks(64).filter_map(|c| std::str::from_utf8(c).ok()).collect();
    format!("-----BEGIN {label}-----\n{}\n-----END {label}-----\n", lines.join("\n"))
}

/// The DER of `text`'s first PEM block under `label`.
pub fn der_of(text: &str, label: &str) -> Option<Vec<u8>> {
    let (_, rest) = text.split_once(format!("-----BEGIN {label}-----").as_str())?;
    let (body, _) = rest.split_once(format!("-----END {label}-----").as_str())?;
    B64.decode(body.split_whitespace().collect::<String>()).ok()
}

/// DSSE's pre-authentication encoding of `payload` as `payload_type`: what an envelope's
/// signatures sign.
pub fn pae(payload_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = format!("DSSEv1 {} {payload_type} {} ", payload_type.len(), payload.len()).into_bytes();
    out.extend_from_slice(payload);
    out
}

/// A DER element of `tag` around `content`.
fn der(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    match u8::try_from(content.len()) {
        Ok(n) if n < 0x80 => out.push(n),
        _ => {
            let len: Vec<u8> = content.len().to_be_bytes().into_iter().skip_while(|b| *b == 0).collect();
            out.push(0x80 | u8::try_from(len.len()).unwrap_or(0));
            out.extend(len);
        }
    }
    out.extend_from_slice(content);
    out
}

/// An identity token's claims (a JWT's middle part), read as the simulator reads them: unsigned.
pub fn claims_of(token: &str) -> Option<Value> {
    let mut parts = token.split('.');
    let (_, payload, _) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    serde_json::from_slice::<Value>(&B64URL.decode(payload.trim_end_matches('=')).ok()?).ok().filter(Value::is_object)
}

/// An identity token naming `issuer` and `subject`. Unsigned: the simulator doesn't check its
/// issuer's signature, as a Fulcio does.
pub fn token_of(issuer: &str, subject: &str) -> String {
    let part = |v: Value| B64URL.encode(v.to_string());
    format!("{}.{}.{}", part(json!({ "alg": "ES256", "typ": "JWT" })), part(json!({ "aud": "sigstore", "iss": issuer, "sub": subject })), B64URL.encode("unsigned"))
}

/// A key the simulator verifies with: ECDSA on P-256 (SHA-256) or P-384 (SHA-384), or Ed25519.
enum PublicKey {
    P256(Vec<u8>),
    P384(Vec<u8>),
    Ed25519(Vec<u8>),
}

impl PublicKey {
    fn of_spki(der: &[u8]) -> Result<PublicKey, String> {
        let (_, spki) = SubjectPublicKeyInfo::from_der(der).map_err(|e| format!("it isn't a SubjectPublicKeyInfo: {e}"))?;
        let bits = spki.subject_public_key.data.to_vec();
        match spki.algorithm.algorithm.to_id_string().as_str() {
            "1.2.840.10045.2.1" => match spki.algorithm.parameters.as_ref().and_then(|p| p.as_oid().ok()).map(|o| o.to_id_string()).as_deref() {
                Some("1.2.840.10045.3.1.7") => Ok(PublicKey::P256(bits)),
                Some("1.3.132.0.34") => Ok(PublicKey::P384(bits)),
                other => Err(format!("an EC key on curve {other:?}: P-256 and P-384 are taken")),
            },
            "1.3.101.112" => Ok(PublicKey::Ed25519(bits)),
            other => Err(format!("a {other} key: ECDSA and Ed25519 keys are taken")),
        }
    }

    fn verifies(&self, message: &[u8], signature: &[u8]) -> bool {
        let (alg, bytes): (&'static dyn VerificationAlgorithm, &[u8]) = match self {
            PublicKey::P256(b) => (&sig::ECDSA_P256_SHA256_ASN1, b),
            PublicKey::P384(b) => (&sig::ECDSA_P384_SHA384_ASN1, b),
            PublicKey::Ed25519(b) => (&sig::ED25519, b),
        };
        UnparsedPublicKey::new(alg, bytes).verify(message, signature).is_ok()
    }
}

/// A Rekor verifier's key (the verifier is base64 of a PEM certificate or public key).
fn verifier_key(b64_pem: &str) -> Option<PublicKey> {
    let text = String::from_utf8(B64.decode(b64_pem).ok()?).ok()?;
    if let Some(der) = der_of(&text, "CERTIFICATE") {
        let (_, cert) = X509Certificate::from_der(&der).ok()?;
        return PublicKey::of_spki(cert.public_key().raw).ok();
    }
    PublicKey::of_spki(&der_of(&text, "PUBLIC KEY")?).ok()
}

// ------------------------------------------------------------------ RFC 6962's tree

fn node(left: &[u8], right: &[u8]) -> [u8; 32] {
    let mut b = vec![1u8];
    b.extend_from_slice(left);
    b.extend_from_slice(right);
    sha256(&b)
}

fn leaf_hash(body: &[u8]) -> [u8; 32] {
    let mut b = vec![0u8];
    b.extend_from_slice(body);
    sha256(&b)
}

/// The largest power of two below `n` (for `n` above one).
fn split(n: usize) -> usize {
    let mut k = 1usize;
    while k << 1 < n {
        k <<= 1;
    }
    k
}

/// The tree hash of `leaves` (their leaf hashes).
fn tree_hash(leaves: &[[u8; 32]]) -> [u8; 32] {
    match leaves {
        [] => sha256(b""),
        [one] => *one,
        _ => match leaves.split_at_checked(split(leaves.len())) {
            Some((l, r)) => node(&tree_hash(l), &tree_hash(r)),
            None => sha256(b""),
        },
    }
}

/// The audit path of leaf `m` in `leaves`, from the leaf up.
fn path(m: usize, leaves: &[[u8; 32]]) -> Vec<[u8; 32]> {
    let k = split(leaves.len());
    let Some((l, r)) = leaves.split_at_checked(k).filter(|_| leaves.len() > 1) else { return Vec::new() };
    let (mut p, other) = if m < k { (path(m, l), tree_hash(r)) } else { (path(m.saturating_sub(k), r), tree_hash(l)) };
    p.push(other);
    p
}

// ------------------------------------------------------------------ the simulator

/// An Ed25519 key's PKCS#8 (RFC 8410's: the seed alone).
fn ed25519_pkcs8(seed: &[u8; 32]) -> Vec<u8> {
    let mut der = vec![0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20];
    der.extend_from_slice(seed);
    der
}

/// An Ed25519 public key's SubjectPublicKeyInfo.
fn ed25519_spki(public: &[u8]) -> Vec<u8> {
    let mut der = vec![0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];
    der.extend_from_slice(public);
    der
}

/// A signed note's key hint for an Ed25519 key: the first four bytes of the SHA-256 of its name, a
/// newline, its algorithm (1) and the key.
fn key_hint(name: &str, public: &[u8]) -> Vec<u8> {
    let mut b = format!("{name}\n").into_bytes();
    b.push(1);
    b.extend_from_slice(public);
    sha256(&b).into_iter().take(4).collect()
}

struct Keys {
    ca: rcgen::Issuer<'static, rcgen::KeyPair>,
    ca_der: Vec<u8>,
    log: Ed25519KeyPair,
    log_spki: Vec<u8>,
    log_id: [u8; 32],
    origin: String,
}

impl Keys {
    fn new(seed: u64) -> Result<Keys, String> {
        let seed_of = |what: &str| sha256(format!("kiln-seams SimSigstore {what} {seed}").as_bytes());
        let ca_err = |e: rcgen::Error| format!("the simulated Fulcio's CA: {e}");
        let ca_key = rcgen::KeyPair::try_from(ed25519_pkcs8(&seed_of("Fulcio")).as_slice()).map_err(ca_err)?;
        let mut p = rcgen::CertificateParams::new(Vec::<String>::new()).map_err(ca_err)?;
        p.distinguished_name = rcgen::DistinguishedName::new();
        p.distinguished_name.push(rcgen::DnType::OrganizationName, "kiln-seams");
        p.distinguished_name.push(rcgen::DnType::CommonName, "SimSigstore Fulcio");
        p.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        p.key_usages = vec![rcgen::KeyUsagePurpose::KeyCertSign, rcgen::KeyUsagePurpose::CrlSign];
        p.not_before = rcgen::date_time_ymd(2020, 1, 1);
        p.not_after = rcgen::date_time_ymd(2100, 1, 1);
        p.serial_number = Some(rcgen::SerialNumber::from_slice(&[0x01]));
        let ca_der = p.self_signed(&ca_key).map_err(ca_err)?.der().to_vec();
        let log = Ed25519KeyPair::from_pkcs8_maybe_unchecked(&ed25519_pkcs8(&seed_of("Rekor"))).map_err(|e| format!("the simulated Rekor's key: {e}"))?;
        let log_spki = ed25519_spki(log.public_key().as_ref());
        let log_id = sha256(&log_spki);
        Ok(Keys { ca: rcgen::Issuer::new(p, ca_key), ca_der, log, log_spki, log_id, origin: format!("{LOG_NAME} - {seed}") })
    }
}

#[derive(Clone)]
struct Entry {
    uuid: String,
    body: String,
    integrated_time: i64,
    promise: String,
}

#[derive(Default)]
struct State {
    entries: Vec<Entry>,
    leaves: Vec<[u8; 32]>,
    by_uuid: HashMap<String, u64>,
    ticks: i64,
    issued: u64,
}

/// A Fulcio and Rekor in memory, whose answers verify against its own trust
/// ([`SimSigstore::trusted_root`]): Fulcio's certificates are for code signing, name the token's
/// issuer and subject, and are valid ten minutes; Rekor keeps dsse entries as Rekor does, in an RFC
/// 6962 tree, numbered from 0.
#[derive(Clone)]
pub struct SimSigstore {
    start: i64,
    keys: Arc<Result<Keys, String>>,
    st: Arc<Mutex<State>>,
}

impl SimSigstore {
    pub fn new(start: i64) -> Self {
        Self::seeded(start, 0)
    }

    /// One whose keys come from `seed`: Sigstores of two seeds trust nothing of each other's.
    pub fn seeded(start: i64, seed: u64) -> Self {
        Self { start, keys: Arc::new(Keys::new(seed)), st: Arc::default() }
    }

    fn keys(&self) -> Result<&Keys, Refusal> {
        self.keys.as_ref().as_ref().map_err(|e| Refusal::Unavailable(e.clone()))
    }

    /// The time of a request: a second after the last one's, from the start.
    fn tick(&self, st: &mut State) -> i64 {
        let t = self.start.saturating_add(st.ticks);
        st.ticks = st.ticks.saturating_add(1);
        t
    }

    /// The index of the entry `uuid` names, when it's logged.
    pub fn index_of(&self, uuid: &str) -> Option<u64> {
        self.st.lock().by_uuid.get(uuid).copied()
    }

    /// Sigstore's trust in this simulator, as a `trusted_root.json` has it: its Fulcio's root and
    /// its log's key, each trusted from 2020, and no CT log (so a verifier asks no SCT of its
    /// certificates); `fulcio` and `rekor` are where they're served.
    pub fn trusted_root(&self, fulcio: &str, rekor: &str) -> Result<Value, Refusal> {
        let k = self.keys()?;
        Ok(json!({
            "mediaType": "application/vnd.dev.sigstore.trustedroot+json;version=0.1",
            "tlogs": [{
                "baseUrl": rekor, "hashAlgorithm": "SHA2_256",
                "publicKey": { "rawBytes": B64.encode(&k.log_spki), "keyDetails": "PKIX_ED25519", "validFor": { "start": "2020-01-01T00:00:00Z" } },
                "logId": { "keyId": B64.encode(k.log_id) },
            }],
            "certificateAuthorities": [{
                "subject": { "organization": "kiln-seams", "commonName": "SimSigstore Fulcio" }, "uri": fulcio,
                "certChain": { "certificates": [{ "rawBytes": B64.encode(&k.ca_der) }] }, "validFor": { "start": "2020-01-01T00:00:00Z" },
            }],
            "ctlogs": [],
            "timestampAuthorities": [],
        }))
    }

    /// Entry `index` as Rekor answers for it: its promise, and the proof of its inclusion in the
    /// tree as it is now, with a checkpoint of that tree the log signed.
    fn answer(&self, st: &State, k: &Keys, index: u64) -> Result<Logged, Refusal> {
        let at = usize::try_from(index).map_err(|_| Refusal::NotFound { index })?;
        let e = st.entries.get(at).ok_or(Refusal::NotFound { index })?;
        let size = st.leaves.len();
        let hashes: Vec<String> = path(at, &st.leaves).iter().map(hex::encode).collect();
        let root = tree_hash(&st.leaves);
        let note = format!("{}\n{size}\n{}\n", k.origin, B64.encode(root));
        let mut hinted = key_hint(LOG_NAME, k.log.public_key().as_ref());
        hinted.extend_from_slice(k.log.sign(note.as_bytes()).as_ref());
        let checkpoint = format!("{note}\n\u{2014} {LOG_NAME} {}\n", B64.encode(hinted));
        let answer = json!({
            "body": e.body, "integratedTime": e.integrated_time, "logID": hex::encode(k.log_id), "logIndex": index,
            "verification": {
                "inclusionProof": { "checkpoint": checkpoint, "hashes": hashes, "logIndex": index, "rootHash": hex::encode(root), "treeSize": size },
                "signedEntryTimestamp": e.promise,
            },
        });
        Ok(Logged { uuid: e.uuid.clone(), index, integrated_time: e.integrated_time, body: e.body.clone(), answer })
    }
}

impl Sigstore for SimSigstore {
    fn certify(&self, ask: CertifyAsk) -> BoxFuture<'_, Result<Certified, Refusal>> {
        Box::pin(async move {
            let k = self.keys()?;
            let claims = claims_of(&ask.token).ok_or_else(|| Refusal::Unauthorized("the identity token isn't a JWT".into()))?;
            let claim = |c: &str| claims.get(c).and_then(Value::as_str).filter(|s| !s.is_empty());
            let (Some(iss), Some(sub)) = (claim("iss"), claim("sub")) else {
                return Err(Refusal::Unauthorized("the identity token names no issuer or no subject".into()));
            };
            let email = claim("email");
            let spki = der_of(&ask.public_key_pem, "PUBLIC KEY").ok_or_else(|| Refusal::Invalid { what: "public key", why: "it isn't a PEM public key".into() })?;
            let key = PublicKey::of_spki(&spki).map_err(|why| Refusal::Invalid { what: "public key", why })?;
            if !key.verifies(email.unwrap_or(sub).as_bytes(), &ask.proof) {
                return Err(Refusal::Invalid { what: "proof", why: "it isn't the key's signature of the token's subject".into() });
            }
            let fail = |e: rcgen::Error| Refusal::Unavailable(format!("the simulated Fulcio: {e}"));
            let san = match email {
                Some(m) => m.try_into().map(rcgen::SanType::Rfc822Name),
                None => sub.try_into().map(rcgen::SanType::URI),
            }
            .map_err(|e: rcgen::Error| Refusal::Unauthorized(format!("the token's subject: {e}")))?;
            let public = rcgen::SubjectPublicKeyInfo::from_der(&spki).map_err(|e| Refusal::Invalid { what: "public key", why: e.to_string() })?;
            let (now, serial) = {
                let mut st = self.st.lock();
                st.issued = st.issued.saturating_add(1);
                (self.tick(&mut st), st.issued)
            };
            let at = |t: i64| time::OffsetDateTime::from_unix_timestamp(t).map_err(|e| Refusal::Unavailable(format!("the simulated Fulcio's clock: {e}")));
            let mut p = rcgen::CertificateParams::new(Vec::<String>::new()).map_err(fail)?;
            p.distinguished_name = rcgen::DistinguishedName::new();
            p.subject_alt_names = vec![san];
            p.is_ca = rcgen::IsCa::ExplicitNoCa;
            p.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
            p.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::CodeSigning];
            p.not_before = at(now)?;
            p.not_after = at(now.saturating_add(SIM_CERT_SECONDS))?;
            let mut number = vec![0x40];
            number.extend(serial.to_be_bytes());
            p.serial_number = Some(rcgen::SerialNumber::from_slice(&number));
            p.custom_extensions = vec![rcgen::CustomExtension::from_oid_content(&ISSUER_V1, iss.as_bytes().to_vec()), rcgen::CustomExtension::from_oid_content(&ISSUER_V2, der(0x0c, iss.as_bytes()))];
            let leaf = p.signed_by(&public, &k.ca).map_err(fail)?;
            Ok(Certified { chain: vec![leaf.pem(), pem("CERTIFICATE", &k.ca_der)] })
        })
    }

    fn log(&self, entry: Value) -> BoxFuture<'_, Result<Logged, Refusal>> {
        Box::pin(async move {
            let k = self.keys()?;
            if self.st.lock().entries.len() >= SIM_ENTRIES {
                return Err(Refusal::Full { limit: SIM_ENTRIES });
            }
            let bad = |why: &str| Refusal::Invalid { what: "entry", why: why.to_owned() };
            if entry.get("kind").and_then(Value::as_str) != Some("dsse") || entry.get("apiVersion").and_then(Value::as_str) != Some("0.0.1") {
                return Err(bad("it isn't a dsse 0.0.1 entry"));
            }
            let envelope = entry.pointer("/spec/proposedContent/envelope").and_then(Value::as_str).ok_or_else(|| bad("it has no envelope"))?;
            let verifiers: Vec<(&str, PublicKey)> =
                entry.pointer("/spec/proposedContent/verifiers").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).filter_map(|v| verifier_key(v).map(|key| (v, key))).collect();
            if verifiers.is_empty() {
                return Err(bad("it names no verifier the log can read"));
            }
            let env: Value = serde_json::from_str(envelope).map_err(|_| bad("its envelope isn't JSON"))?;
            let payload_type = env.get("payloadType").and_then(Value::as_str).ok_or_else(|| bad("its envelope has no payload type"))?;
            let payload = env.get("payload").and_then(Value::as_str).and_then(|p| B64.decode(p).ok()).ok_or_else(|| bad("its envelope's payload isn't base64"))?;
            let signed = pae(payload_type, &payload);
            let mut signatures = Vec::new();
            for s in env.get("signatures").and_then(Value::as_array).into_iter().flatten() {
                let text = s.get("sig").and_then(Value::as_str).ok_or_else(|| bad("a signature is missing"))?;
                let raw = B64.decode(text).map_err(|_| bad("a signature isn't base64"))?;
                let (verifier, _) = verifiers.iter().find(|(_, key)| key.verifies(&signed, &raw)).ok_or_else(|| bad("a signature isn't its verifiers'"))?;
                signatures.push(json!({ "signature": text, "verifier": verifier }));
            }
            if signatures.is_empty() {
                return Err(bad("its envelope has no signature"));
            }
            let body = json!({ "apiVersion": "0.0.1", "kind": "dsse", "spec": {
                "envelopeHash": { "algorithm": "sha256", "value": hex::encode(sha256(envelope.as_bytes())) },
                "payloadHash": { "algorithm": "sha256", "value": hex::encode(sha256(&payload)) },
                "signatures": signatures,
            } })
            .to_string();
            let leaf = leaf_hash(body.as_bytes());
            let uuid = hex::encode(leaf);
            let body = B64.encode(&body);
            let mut st = self.st.lock();
            if let Some(index) = st.by_uuid.get(&uuid) {
                return Err(Refusal::Conflict { index: *index });
            }
            let index = u64::try_from(st.entries.len()).map_err(|_| Refusal::Full { limit: SIM_ENTRIES })?;
            let at = self.tick(&mut st);
            // The log's promise: the body, its time and index (canonical JSON, keys in order), signed.
            let promised = format!("{{\"body\":{},\"integratedTime\":{at},\"logID\":\"{}\",\"logIndex\":{index}}}", Value::String(body.clone()), hex::encode(k.log_id));
            let promise = B64.encode(k.log.sign(promised.as_bytes()).as_ref());
            st.leaves.push(leaf);
            st.by_uuid.insert(uuid.clone(), index);
            st.entries.push(Entry { uuid, body, integrated_time: at, promise });
            self.answer(&st, k, index)
        })
    }

    fn entry(&self, index: u64) -> BoxFuture<'_, Result<Logged, Refusal>> {
        Box::pin(async move {
            let k = self.keys()?;
            let st = self.st.lock();
            self.answer(&st, k, index)
        })
    }
}

// ------------------------------------------------------------------ the suite

/// Who asks in a case: an identity token naming an issuer and a subject, and a P-256 key of its
/// own, its proof the key's signature of the subject.
pub struct Asker {
    key: sig::EcdsaKeyPair,
    token: String,
    subject: String,
    public_pem: String,
}

impl Asker {
    pub fn new(issuer: &str, subject: &str) -> Result<Asker, String> {
        use aws_lc_rs::encoding::AsDer as _;
        let doc = sig::EcdsaKeyPair::generate_pkcs8(&sig::ECDSA_P256_SHA256_ASN1_SIGNING, &aws_lc_rs::rand::SystemRandom::new()).map_err(|_| "no key could be made".to_owned())?;
        let key = sig::EcdsaKeyPair::from_pkcs8(&sig::ECDSA_P256_SHA256_ASN1_SIGNING, doc.as_ref()).map_err(|e| format!("the key: {e}"))?;
        let public_pem = pem("PUBLIC KEY", key.public_key().as_der().map_err(|_| "the key's public half".to_owned())?.as_ref());
        Ok(Asker { key, token: token_of(issuer, subject), subject: subject.to_owned(), public_pem })
    }

    /// The key's signature of `message` (none when it couldn't sign).
    pub fn sign(&self, message: &[u8]) -> Vec<u8> {
        self.key.sign(&aws_lc_rs::rand::SystemRandom::new(), message).map(|s| s.as_ref().to_vec()).unwrap_or_default()
    }

    /// The token, the key and its proof, to certify.
    pub fn ask(&self) -> CertifyAsk {
        CertifyAsk { token: self.token.clone(), public_key_pem: self.public_pem.clone(), proof: self.sign(self.subject.as_bytes()) }
    }
}

/// What a case needs: a fresh Fulcio and Rekor, and who asks of them.
pub trait SigstoreFixture: Send + Sync {
    fn sigstore(&self) -> &dyn Sigstore;
    /// Who asks: a token the implementation takes, and a key of its own.
    fn asker(&self) -> Result<&Asker, Failure>;
}

pub struct SimSigstoreFixture {
    pub sim: SimSigstore,
    asker: Result<Asker, String>,
}

impl Default for SimSigstoreFixture {
    fn default() -> Self {
        Self { sim: SimSigstore::new(1_790_000_000), asker: Asker::new(ISSUER, SUBJECT) }
    }
}

impl SigstoreFixture for SimSigstoreFixture {
    fn sigstore(&self) -> &dyn Sigstore {
        &self.sim
    }
    fn asker(&self) -> Result<&Asker, Failure> {
        self.asker.as_ref().map_err(Clone::clone)
    }
}

/// The asker's key certified: the certificate's PEM.
async fn verifier<F: SigstoreFixture>(f: &F) -> Result<String, Failure> {
    let got = fine("certify", f.sigstore().certify(f.asker()?.ask()).await)?;
    got.chain.first().cloned().ok_or_else(|| "a chain with no certificate".to_owned())
}

/// A dsse entry of a statement `n` makes its own, signed by `a`, its verifier `leaf` (a PEM).
fn entry_of(a: &Asker, leaf: &str, n: u64) -> Value {
    let payload = json!({ "_type": "https://in-toto.io/Statement/v1", "predicate": { "n": n }, "predicateType": "https://kiln-seams.test/conformance", "subject": [] }).to_string();
    let envelope = json!({ "payload": B64.encode(&payload), "payloadType": IN_TOTO, "signatures": [{ "keyid": "", "sig": B64.encode(a.sign(&pae(IN_TOTO, payload.as_bytes()))) }] });
    json!({ "apiVersion": "0.0.1", "kind": "dsse", "spec": { "proposedContent": { "envelope": envelope.to_string(), "verifiers": [B64.encode(leaf)] } } })
}

fn c1<F: SigstoreFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let ask = f.asker()?.ask();
        let got = fine("certify", f.sigstore().certify(ask.clone()).await)?;
        ensure(got.chain.len() >= 2, || format!("a chain of {}: a leaf and what issued it, at least", got.chain.len()))?;
        ensure(got.chain.iter().all(|c| c.contains("-----BEGIN CERTIFICATE-----")), || "every link a PEM certificate".into())?;
        let der = got.chain.first().and_then(|c| der_of(c, "CERTIFICATE")).ok_or_else(|| "the leaf isn't PEM".to_owned())?;
        let (_, leaf) = X509Certificate::from_der(&der).map_err(|e| format!("the leaf isn't a certificate: {e}"))?;
        same("the leaf's key", Some(leaf.public_key().raw.to_vec()), der_of(&ask.public_key_pem, "PUBLIC KEY"))?;
        ensure(leaf.extended_key_usage().ok().flatten().is_some_and(|u| u.value.code_signing), || "the leaf isn't for code signing".into())?;
        let iss = claims_of(&ask.token).and_then(|c| c.get("iss").and_then(Value::as_str).map(str::to_owned)).unwrap_or_default();
        let names = leaf.extensions().iter().any(|e| {
            let oid = e.oid.to_id_string();
            (oid == "1.3.6.1.4.1.57264.1.8" && e.value.first() == Some(&0x0c) && e.value.ends_with(iss.as_bytes())) || (oid == "1.3.6.1.4.1.57264.1.1" && e.value == iss.as_bytes())
        });
        ensure(names, || format!("the leaf doesn't name the token's issuer {iss}"))
    })
}

fn c2<F: SigstoreFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let a = f.asker()?;
        let good = a.ask();
        match refused("a token that isn't one", f.sigstore().certify(CertifyAsk { token: "not a token".into(), ..good.clone() }).await)? {
            Refusal::Unauthorized(_) => {}
            other => return Err(format!("a token that isn't one: refused {other:?}, want Unauthorized")),
        }
        for (what, ask) in [("public key", CertifyAsk { public_key_pem: "not a key".into(), ..good.clone() }), ("proof", CertifyAsk { proof: a.sign(b"another subject"), ..good.clone() })] {
            match refused(what, f.sigstore().certify(ask).await)? {
                Refusal::Invalid { what: w, .. } => same("what's named", w, what)?,
                other => return Err(format!("{what}: refused {other:?}, want Invalid")),
            }
        }
        Ok(())
    })
}

fn c3<F: SigstoreFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let leaf = verifier(f).await?;
        let logged = fine("log", f.sigstore().log(entry_of(f.asker()?, &leaf, 3)).await)?;
        let again = fine("entry", f.sigstore().entry(logged.index).await)?;
        same("uuid", &again.uuid, &logged.uuid)?;
        same("body", &again.body, &logged.body)?;
        same("time", again.integrated_time, logged.integrated_time)
    })
}

fn c4<F: SigstoreFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let leaf = verifier(f).await?;
        let mut last: Option<(u64, i64)> = None;
        for n in 40..45 {
            let l = fine("log", f.sigstore().log(entry_of(f.asker()?, &leaf, n)).await)?;
            if let Some((i, t)) = last {
                ensure(l.index > i, || format!("index {} after {i}", l.index))?;
                ensure(l.integrated_time >= t, || format!("time {} after {t}", l.integrated_time))?;
            }
            last = Some((l.index, l.integrated_time));
        }
        Ok(())
    })
}

fn c5<F: SigstoreFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let leaf = verifier(f).await?;
        let entry = entry_of(f.asker()?, &leaf, 5);
        let first = fine("log", f.sigstore().log(entry.clone()).await)?;
        match refused("the same entry again", f.sigstore().log(entry).await)? {
            Refusal::Conflict { index } => same("the index named", index, first.index),
            other => Err(format!("refused {other:?}, want Conflict")),
        }
    })
}

fn c6<F: SigstoreFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        match refused("an index never logged", f.sigstore().entry(u64::MAX >> 1).await)? {
            Refusal::NotFound { .. } => Ok(()),
            other => Err(format!("refused {other:?}, want NotFound")),
        }
    })
}

fn c7<F: SigstoreFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let leaf = verifier(f).await?;
        let logged = fine("log", f.sigstore().log(entry_of(f.asker()?, &leaf, 70)).await)?;
        fine("log", f.sigstore().log(entry_of(f.asker()?, &leaf, 71)).await)?;
        let again = fine("entry", f.sigstore().entry(logged.index).await)?;
        for (what, l) in [("as logged", &logged), ("as found again", &again)] {
            let v = &l.answer;
            ensure(v.pointer("/verification/signedEntryTimestamp").and_then(Value::as_str).is_some_and(|s| !s.is_empty()), || format!("{what}: no signed entry timestamp"))?;
            let p = v.pointer("/verification/inclusionProof").ok_or_else(|| format!("{what}: no inclusion proof"))?;
            same(&format!("{what}: the proof's index"), p.get("logIndex").and_then(Value::as_u64), Some(l.index))?;
            let size = p.get("treeSize").and_then(Value::as_u64).unwrap_or(0);
            ensure(size > l.index, || format!("{what}: a tree of {size} holding entry {}", l.index))?;
            let root = p.get("rootHash").and_then(Value::as_str).and_then(|h| hex::decode(h).ok()).ok_or_else(|| format!("{what}: no root hash"))?;
            let checkpoint: Vec<String> = p.get("checkpoint").and_then(Value::as_str).unwrap_or_default().lines().map(str::to_owned).collect();
            same(&format!("{what}: the checkpoint's tree size"), checkpoint.get(1).cloned(), Some(size.to_string()))?;
            same(&format!("{what}: the checkpoint's root"), checkpoint.get(2).cloned(), Some(B64.encode(&root)))?;
        }
        Ok(())
    })
}

fn c8<F: SigstoreFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let leaf = verifier(f).await?;
        let a = f.asker()?;
        let envelope = json!({ "payload": B64.encode("{}"), "payloadType": IN_TOTO, "signatures": [{ "keyid": "", "sig": B64.encode(a.sign(b"another envelope")) }] });
        let entry = json!({ "apiVersion": "0.0.1", "kind": "dsse", "spec": { "proposedContent": { "envelope": envelope.to_string(), "verifiers": [B64.encode(&leaf)] } } });
        match refused("an entry its verifier didn't sign", f.sigstore().log(entry).await)? {
            Refusal::Invalid { what, .. } => same("what's named", what, "entry"),
            other => Err(format!("refused {other:?}, want Invalid")),
        }
    })
}

/// The Sigstore suite: docs/seams.md's clauses, in order.
pub fn cases<F: SigstoreFixture>() -> Vec<Case<F>> {
    vec![
        Case { id: "SIGSTORE-1", says: "a key with a token it takes is certified: a chain of PEM certificates, the key's first (for code signing, naming the token's issuer), then what issued it", check: c1::<F> },
        Case { id: "SIGSTORE-2", says: "a token that isn't one is refused Unauthorized; a key that isn't one, or a proof that isn't the key's, is refused Invalid, naming which", check: c2::<F> },
        Case { id: "SIGSTORE-3", says: "an entry logged is found again at its index, with the same uuid, body and time", check: c3::<F> },
        Case { id: "SIGSTORE-4", says: "entries are numbered in the order they're logged, and their times never go back", check: c4::<F> },
        Case { id: "SIGSTORE-5", says: "the same entry logged twice is refused Conflict, naming where it is", check: c5::<F> },
        Case { id: "SIGSTORE-6", says: "an index never logged is refused NotFound", check: c6::<F> },
        Case {
            id: "SIGSTORE-7",
            says: "an entry comes with the log's promise and the proof of its inclusion: its index in a tree of more entries than that, the tree's root, and a checkpoint of that tree",
            check: c7::<F>,
        },
        Case { id: "SIGSTORE-8", says: "an entry whose signature isn't its verifier's is refused Invalid, naming the entry", check: c8::<F> },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::run;

    #[test]
    fn the_simulated_sigstore_passes_the_sigstore_suite() -> Result<(), String> {
        futures::executor::block_on(run("Sigstore", &cases::<SimSigstoreFixture>(), || async { SimSigstoreFixture::default() })).into_result().map(|_| ())
    }

    #[test]
    fn its_log_is_bounded_its_tree_rfc_6962s_and_its_keys_its_seeds() {
        let l = |s: &str| leaf_hash(s.as_bytes());
        let (a, b, c) = (l("a"), l("b"), l("c"));
        assert_eq!(tree_hash(&[a, b, c]), node(&node(&a, &b), &c));
        assert_eq!(path(0, &[a, b, c]), vec![b, c]);
        assert_eq!(path(2, &[a, b, c]), vec![node(&a, &b)]);
        assert_eq!(path(0, &[a]), Vec::<[u8; 32]>::new());
        let s = SimSigstore::new(0);
        s.st.lock().entries = vec![Entry { uuid: String::new(), body: String::new(), integrated_time: 0, promise: String::new() }; SIM_ENTRIES];
        assert_eq!(futures::executor::block_on(s.log(json!({}))), Err(Refusal::Full { limit: SIM_ENTRIES }));
        let root = |s: &SimSigstore| s.trusted_root("f", "r").map(|v| v.to_string());
        assert_eq!(root(&SimSigstore::new(5)), root(&SimSigstore::new(9)), "the same seed, the same keys");
        assert_ne!(root(&SimSigstore::new(0)), root(&SimSigstore::seeded(0, 1)), "another seed, other keys");
    }
}
