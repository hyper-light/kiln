//! This runner process's identities (docs/ci.md W27-R01, R02): keys it makes and keeps in its own
//! memory, never written or sent, and the certificates kiln's CA issues for them from CSRs
//! ([`crate::proto::AgentMsg::Identity`]). Each is held under what it's for:
//!
//! - `runner`: the process itself, made once per process and certified again on each connection;
//! - `agent/<session>/<epoch>`: an agent session it holds, at an epoch;
//! - `leg/<leg>`: a CI leg it runs.
//!
//! A certificate (public) is also written under `<state>/identities/`, for whoever wants to see
//! it; the key never is.

use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

use parking_lot::Mutex;

use crate::proto::{IdentityAsk, IdentityGiven};

#[derive(Default)]
struct Held {
    /// Private keys (PEM), by what they're for. Only in this process's memory.
    keys: HashMap<String, String>,
    /// Certificates (PEM), by what they're for.
    certs: HashMap<String, String>,
}

static HELD: LazyLock<Mutex<Held>> = LazyLock::new(Default::default);

/// What an identity is held under.
pub fn slot(kind: &str, of: Option<&str>, epoch: Option<u64>) -> String {
    match (of, epoch) {
        (Some(of), Some(e)) => format!("{kind}/{of}/{e}"),
        (Some(of), None) => format!("{kind}/{of}"),
        _ => kind.to_owned(),
    }
}

/// The request for an identity: a CSR for the slot's key, made here the first time it's asked.
pub fn ask(kind: &str, of: Option<&str>, epoch: Option<u64>) -> anyhow::Result<String> {
    let slot = slot(kind, of, epoch);
    let csr = {
        let mut held = HELD.lock();
        let key = match held.keys.get(&slot) {
            Some(k) => k.clone(),
            None => {
                let k = crate::pki::new_key_pem()?;
                held.keys.insert(slot.clone(), k.clone());
                // A test build copies it out, to look for it where it must never be.
                crate::runner::chaos::made_key(&slot, &k);
                k
            }
        };
        crate::pki::make_csr_with_key("kiln-identity", &key)?
    };
    Ok(serde_json::to_string(&IdentityAsk { kind: kind.to_owned(), csr, of: of.map(str::to_owned), epoch })?)
}

/// The server's answer: the certificate kept for its slot (and written, public, under
/// `<state>/identities/<file>.pem`, `file` named by `label`), or why there's none, logged.
pub fn given(json: &str, state_dir: &Path, label: &str) -> anyhow::Result<()> {
    let g: IdentityGiven = serde_json::from_str(json)?;
    let slot = slot(&g.kind, g.of.as_deref(), g.epoch);
    let Some(pem) = g.cert_pem else {
        tracing::warn!(identity = %slot, error = %g.error.unwrap_or_default(), "kiln issued no identity");
        return Ok(());
    };
    let dir = state_dir.join("identities");
    std::fs::create_dir_all(&dir)?;
    let file: String = format!("{label}-{slot}").chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_') { c } else { '_' }).collect();
    crate::util::write_secret(&dir.join(format!("{file}.pem")), pem.as_bytes(), 0o644)?;
    tracing::info!(identity = %g.uri, "identity certified");
    HELD.lock().certs.insert(slot, pem);
    Ok(())
}

/// The certificate held for a slot.
pub fn cert(slot: &str) -> Option<String> {
    HELD.lock().certs.get(slot).cloned()
}

/// Signs `message` with the slot's key (ECDSA P-256, SHA-256, ASN.1): the signature, and the
/// certificate it verifies under. The key stays here.
pub fn sign(slot: &str, message: &[u8]) -> anyhow::Result<(Vec<u8>, String)> {
    use rcgen::SigningKey as _;
    let held = HELD.lock();
    let key = held.keys.get(slot).ok_or_else(|| anyhow::anyhow!("no key for {slot}"))?;
    let cert = held.certs.get(slot).cloned().ok_or_else(|| anyhow::anyhow!("no certificate for {slot} yet"))?;
    let kp = rcgen::KeyPair::from_pem(key)?;
    Ok((kp.sign(message)?, cert))
}

/// Lets go of every slot starting with `prefix` (`agent/<session>/`: a session that closed).
pub fn forget_prefix(prefix: &str) {
    let mut held = HELD.lock();
    held.keys.retain(|k, _| !k.starts_with(prefix));
    held.certs.retain(|k, _| !k.starts_with(prefix));
}

/// Lets go of a slot's key and certificate (a session that left the epoch, a leg that ended).
pub fn forget(slot: &str) {
    let mut held = HELD.lock();
    held.keys.remove(slot);
    held.certs.remove(slot);
}
