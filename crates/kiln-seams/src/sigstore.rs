//! `Sigstore`: an outside Fulcio and Rekor (docs/ci.md W18-R02, W27-R12). Fulcio certifies a key
//! for whoever an identity token names, the key's holder proving it has it; Rekor logs entries,
//! numbered in the order they come, and gives each back by its number. kiln's keyless signing goes
//! through it. The real one is kiln's client of a Fulcio and a Rekor it's configured with
//! (`kiln::ci::signing::Remote`); [`SimSigstore`] certifies with a chain of its own and keeps its
//! log in memory, its times counted from a start it's given, so a run is the same every time.

use std::collections::HashMap;
use std::sync::Arc;

use futures::future::BoxFuture;
use parking_lot::Mutex;
use serde_json::Value;

use crate::conformance::{Case, Failure, ensure, fine, refused, same};

/// The most entries [`SimSigstore`] logs; past it, refused `Full`.
pub const SIM_ENTRIES: usize = 100_000;

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
/// log keeps it, and the log's whole answer for it (its promise and proof of inclusion).
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

/// PEM of `bytes` under `label`, 64 columns.
fn pem(label: &str, bytes: &[u8]) -> String {
    let b64 = b64(bytes);
    let lines: Vec<&str> = b64.as_bytes().chunks(64).filter_map(|c| std::str::from_utf8(c).ok()).collect();
    format!("-----BEGIN {label}-----\n{}\n-----END {label}-----\n", lines.join("\n"))
}

/// Standard base64 (kiln-seams takes no crate for it).
fn b64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let ch = |i: u32| A.get(usize::try_from(i & 63).unwrap_or(0)).map_or('=', |c| char::from(*c));
    let mut out = String::new();
    for c in bytes.chunks(3) {
        let (a, b, d) = (c.first().copied().unwrap_or(0), c.get(1).copied(), c.get(2).copied());
        let n = u32::from(a) << 16 | u32::from(b.unwrap_or(0)) << 8 | u32::from(d.unwrap_or(0));
        out.push(ch(n >> 18));
        out.push(ch(n >> 12));
        out.push(if b.is_some() { ch(n >> 6) } else { '=' });
        out.push(if d.is_some() { ch(n) } else { '=' });
    }
    out
}

#[derive(Default)]
struct Log {
    entries: Vec<Logged>,
    by_uuid: HashMap<String, u64>,
}

/// A Fulcio and Rekor in memory. Its chain is its own (a leaf naming the key's digest and the
/// token's, then its root), not one a Sigstore verifier trusts; its log numbers entries from 0,
/// each taken one second after the last, from `start`.
#[derive(Clone)]
pub struct SimSigstore {
    start: i64,
    log: Arc<Mutex<Log>>,
}

impl SimSigstore {
    pub fn new(start: i64) -> Self {
        Self { start, log: Arc::default() }
    }
}

impl Sigstore for SimSigstore {
    fn certify(&self, ask: CertifyAsk) -> BoxFuture<'_, Result<Certified, Refusal>> {
        Box::pin(async move {
            if ask.token.trim().is_empty() {
                return Err(Refusal::Invalid { what: "token", why: "it's empty".into() });
            }
            if !ask.public_key_pem.contains("-----BEGIN PUBLIC KEY-----") {
                return Err(Refusal::Invalid { what: "public key", why: "it isn't a PEM public key".into() });
            }
            if ask.proof.is_empty() {
                return Err(Refusal::Invalid { what: "proof", why: "it's empty".into() });
            }
            let leaf = format!("kiln-seams SimSigstore certificate for key {} and token {}", crate::sha256_hex(ask.public_key_pem.as_bytes()), crate::sha256_hex(ask.token.as_bytes()));
            Ok(Certified { chain: vec![pem("CERTIFICATE", leaf.as_bytes()), pem("CERTIFICATE", b"kiln-seams SimSigstore root")] })
        })
    }

    fn log(&self, entry: Value) -> BoxFuture<'_, Result<Logged, Refusal>> {
        Box::pin(async move {
            if !entry.is_object() || entry.get("kind").and_then(Value::as_str).is_none() || entry.get("spec").is_none() {
                return Err(Refusal::Invalid { what: "entry", why: "it needs a kind and a spec".into() });
            }
            let body = entry.to_string();
            let uuid = crate::sha256_hex(body.as_bytes());
            let mut log = self.log.lock();
            if let Some(index) = log.by_uuid.get(&uuid) {
                return Err(Refusal::Conflict { index: *index });
            }
            if log.entries.len() >= SIM_ENTRIES {
                return Err(Refusal::Full { limit: SIM_ENTRIES });
            }
            let index = u64::try_from(log.entries.len()).map_err(|_| Refusal::Full { limit: SIM_ENTRIES })?;
            let integrated_time = self.start.saturating_add(i64::try_from(index).unwrap_or(i64::MAX));
            let answer = serde_json::json!({ "logIndex": index, "integratedTime": integrated_time, "body": body.clone(), "logID": crate::sha256_hex(b"kiln-seams SimSigstore log") });
            let logged = Logged { uuid: uuid.clone(), index, integrated_time, body, answer };
            log.by_uuid.insert(uuid, index);
            log.entries.push(logged.clone());
            Ok(logged)
        })
    }

    fn entry(&self, index: u64) -> BoxFuture<'_, Result<Logged, Refusal>> {
        Box::pin(async move {
            let log = self.log.lock();
            usize::try_from(index).ok().and_then(|i| log.entries.get(i)).cloned().ok_or(Refusal::NotFound { index })
        })
    }
}

/// What a case needs: a fresh Fulcio and Rekor, and a token and key they take.
pub trait SigstoreFixture: Send + Sync {
    fn sigstore(&self) -> &dyn Sigstore;
    /// A token, a key (PEM) and a proof the implementation accepts.
    fn ask(&self) -> CertifyAsk;
}

pub struct SimSigstoreFixture(pub SimSigstore);

impl Default for SimSigstoreFixture {
    fn default() -> Self {
        Self(SimSigstore::new(1_790_000_000))
    }
}

impl SigstoreFixture for SimSigstoreFixture {
    fn sigstore(&self) -> &dyn Sigstore {
        &self.0
    }
    fn ask(&self) -> CertifyAsk {
        CertifyAsk { token: "eyJ.sim.token".into(), public_key_pem: pem("PUBLIC KEY", b"a key"), proof: vec![1, 2, 3] }
    }
}

/// A proposed entry no other case logs: `n` makes it its own.
fn entry_of(n: u64) -> Value {
    serde_json::json!({ "apiVersion": "0.0.1", "kind": "dsse", "spec": { "proposedContent": { "envelope": format!("{{\"n\":{n},\"at\":\"kiln-seams conformance\"}}"), "verifiers": [] } } })
}

fn c1<F: SigstoreFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let got = fine("certify", f.sigstore().certify(f.ask()).await)?;
        ensure(got.chain.len() >= 2, || format!("a chain of {}: a leaf and what issued it, at least", got.chain.len()))?;
        ensure(got.chain.iter().all(|c| c.contains("-----BEGIN CERTIFICATE-----")), || "every link a PEM certificate".into())
    })
}

fn c2<F: SigstoreFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let good = f.ask();
        for (what, ask) in [
            ("token", CertifyAsk { token: String::new(), ..good.clone() }),
            ("public key", CertifyAsk { public_key_pem: "not a key".into(), ..good.clone() }),
            ("proof", CertifyAsk { proof: Vec::new(), ..good.clone() }),
        ] {
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
        let logged = fine("log", f.sigstore().log(entry_of(3)).await)?;
        let again = fine("entry", f.sigstore().entry(logged.index).await)?;
        same("uuid", &again.uuid, &logged.uuid)?;
        same("body", &again.body, &logged.body)?;
        same("time", again.integrated_time, logged.integrated_time)
    })
}

fn c4<F: SigstoreFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let mut last: Option<(u64, i64)> = None;
        for n in 40..45 {
            let l = fine("log", f.sigstore().log(entry_of(n)).await)?;
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
        let first = fine("log", f.sigstore().log(entry_of(5)).await)?;
        match refused("the same entry again", f.sigstore().log(entry_of(5)).await)? {
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

/// The Sigstore suite: docs/seams.md's clauses, in order.
pub fn cases<F: SigstoreFixture>() -> Vec<Case<F>> {
    vec![
        Case { id: "SIGSTORE-1", says: "a key with a token it takes is certified: a chain of PEM certificates, the key's first, then what issued it", check: c1::<F> },
        Case { id: "SIGSTORE-2", says: "a missing token, a key that isn't one or a missing proof is refused Invalid, naming which", check: c2::<F> },
        Case { id: "SIGSTORE-3", says: "an entry logged is found again at its index, with the same uuid, body and time", check: c3::<F> },
        Case { id: "SIGSTORE-4", says: "entries are numbered in the order they're logged, and their times never go back", check: c4::<F> },
        Case { id: "SIGSTORE-5", says: "the same entry logged twice is refused Conflict, naming where it is", check: c5::<F> },
        Case { id: "SIGSTORE-6", says: "an index never logged is refused NotFound", check: c6::<F> },
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
    fn its_log_is_bounded_and_its_base64_is_standard() {
        assert_eq!(b64(b"foobar"), "Zm9vYmFy");
        assert_eq!(b64(b"fooba"), "Zm9vYmE=");
        assert_eq!(b64(b"foob"), "Zm9vYg==");
        let s = SimSigstore::new(0);
        s.log.lock().entries = vec![Logged { uuid: String::new(), index: 0, integrated_time: 0, body: String::new(), answer: Value::Null }; SIM_ENTRIES];
        assert_eq!(futures::executor::block_on(s.log(entry_of(1))), Err(Refusal::Full { limit: SIM_ENTRIES }));
    }
}
