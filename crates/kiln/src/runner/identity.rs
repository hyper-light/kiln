//! This runner process's identities (docs/ci.md W27-R01, R02): keys it makes and keeps in its own
//! memory, never written or sent, and the certificates kiln's CA issues for them from CSRs
//! ([`crate::proto::AgentMsg::Identity`]). Each is held under what it's for:
//!
//! - `runner`: the process itself, made once per process and certified again on each connection;
//! - `agent/<session>/<epoch>`: an agent session it holds, at an epoch;
//! - `leg/<leg>`: a CI leg it runs.
//!
//! A key is aws-lc's alone (ECDSA P-256): never serialized, so no copy of it lies anywhere in this
//! process's memory, and wiped by aws-lc as it's let go of. While the process holds a leg's key,
//! its memory is kept from its user's other processes, the legs' steps among them
//! ([`crate::os::private_memory`]). A certificate (public) is also written under
//! `<state>/identities/`, for whoever wants to see it, while its key is held; the key never is.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::Context as _;
use aws_lc_rs::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair as _};
use parking_lot::Mutex;

use crate::proto::{IdentityAsk, IdentityGiven};

/// What a CI leg's identity is held under (`leg/<leg>`).
const LEG: &str = "leg";

/// A key made here, ECDSA P-256 as kiln's others are. aws-lc holds it and wipes it as it frees
/// it; it's never serialized, so there's no other copy of it to outlive it.
struct Key(EcdsaKeyPair);

impl Key {
    fn new() -> anyhow::Result<Self> {
        EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING).map(Self).map_err(|_| anyhow::anyhow!("a key couldn't be made"))
    }
}

impl rcgen::PublicKeyData for Key {
    fn der_bytes(&self) -> &[u8] {
        self.0.public_key().as_ref()
    }

    fn algorithm(&self) -> &'static rcgen::SignatureAlgorithm {
        &rcgen::PKCS_ECDSA_P256_SHA256
    }
}

impl rcgen::SigningKey for Key {
    fn sign(&self, msg: &[u8]) -> Result<Vec<u8>, rcgen::Error> {
        self.0.sign(&aws_lc_rs::rand::SystemRandom::new(), msg).map(|s| s.as_ref().to_vec()).map_err(|_| rcgen::Error::RingUnspecified)
    }
}

#[derive(Default)]
struct Held {
    /// Keys, by what they're for. Only in this process's memory.
    keys: HashMap<String, Key>,
    /// Certificates (PEM), by what they're for, with where each was written.
    certs: HashMap<String, (String, PathBuf)>,
}

impl Held {
    fn holds_a_leg(&self) -> bool {
        self.keys.keys().any(|s| is_leg(s))
    }

    /// Lets go of the slots `which` picks: their keys (wiped), their certificates and the files
    /// they were written to. Once no leg's key is left, the process's memory is as it was.
    fn forget(&mut self, which: impl Fn(&str) -> bool) {
        let legs = self.holds_a_leg();
        self.keys.retain(|s, _| !which(s));
        let gone: Vec<String> = self.certs.keys().filter(|s| which(s)).cloned().collect();
        for s in gone {
            if let Some((_, file)) = self.certs.remove(&s) {
                let _ = std::fs::remove_file(file);
            }
        }
        if legs
            && !self.holds_a_leg()
            && let Err(e) = crate::os::private_memory(false)
        {
            tracing::warn!(error = %e, "this process's memory is still kept from its user's other processes");
        }
    }
}

fn is_leg(slot: &str) -> bool {
    slot.split('/').next() == Some(LEG)
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
/// A leg's key is kept only once the process's memory is out of its steps' reach (they haven't
/// started: a leg's key is made as the leg is given to this runner).
pub fn ask(kind: &str, of: Option<&str>, epoch: Option<u64>) -> anyhow::Result<String> {
    let slot = slot(kind, of, epoch);
    let csr = {
        let mut held = HELD.lock();
        if !held.keys.contains_key(&slot) {
            let key = Key::new()?;
            if is_leg(&slot) && !held.holds_a_leg() {
                crate::os::private_memory(true).context("keeping this process's memory from the leg's steps")?;
            }
            // A test build's `keys=` rule copies it out (PKCS#8, as PEM), to look for it where it
            // must never be; nothing serializes it otherwise.
            crate::runner::chaos::made_key(&slot, || key.0.to_pkcs8v1().ok().and_then(|d| rcgen::KeyPair::try_from(d.as_ref()).ok()).map(|k| k.serialize_pem()));
            held.keys.insert(slot.clone(), key);
        }
        let key = held.keys.get(&slot).ok_or_else(|| anyhow::anyhow!("no key for {slot}"))?;
        rcgen::CertificateParams::new(vec!["kiln-identity".to_owned()])?.serialize_request(key)?.pem()?
    };
    Ok(serde_json::to_string(&IdentityAsk { kind: kind.to_owned(), csr, of: of.map(str::to_owned), epoch })?)
}

/// The server's answer: the certificate kept for its slot (and written, public, under
/// `<state>/identities/<file>.pem`, `file` named by `label`), or why there's none, logged. One for
/// a key let go of meanwhile (a leg that ended first) is kept nowhere.
pub fn given(json: &str, state_dir: &Path, label: &str) -> anyhow::Result<()> {
    let g: IdentityGiven = serde_json::from_str(json)?;
    let slot = slot(&g.kind, g.of.as_deref(), g.epoch);
    let Some(pem) = g.cert_pem else {
        tracing::warn!(identity = %slot, error = %g.error.unwrap_or_default(), "kiln issued no identity");
        return Ok(());
    };
    let mut held = HELD.lock();
    if !held.keys.contains_key(&slot) {
        tracing::debug!(identity = %g.uri, "a certificate came for a key let go of");
        return Ok(());
    }
    let dir = state_dir.join("identities");
    std::fs::create_dir_all(&dir)?;
    let file: String = format!("{label}-{slot}").chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_') { c } else { '_' }).collect();
    let path = dir.join(format!("{file}.pem"));
    crate::util::write_secret(&path, pem.as_bytes(), 0o644)?;
    tracing::info!(identity = %g.uri, "identity certified");
    held.certs.insert(slot, (pem, path));
    Ok(())
}

/// The certificate held for a slot.
pub fn cert(slot: &str) -> Option<String> {
    HELD.lock().certs.get(slot).map(|(pem, _)| pem.clone())
}

/// Signs `message` with the slot's key (ECDSA P-256, SHA-256, ASN.1): the signature, and the
/// certificate it verifies under. The key stays here.
pub fn sign(slot: &str, message: &[u8]) -> anyhow::Result<(Vec<u8>, String)> {
    use rcgen::SigningKey as _;
    let held = HELD.lock();
    let key = held.keys.get(slot).ok_or_else(|| anyhow::anyhow!("no key for {slot}"))?;
    let (cert, _) = held.certs.get(slot).ok_or_else(|| anyhow::anyhow!("no certificate for {slot} yet"))?;
    Ok((key.sign(message)?, cert.clone()))
}

/// Lets go of every slot starting with `prefix` (`agent/<session>/`: a session that closed).
pub fn forget_prefix(prefix: &str) {
    HELD.lock().forget(|s| s.starts_with(prefix));
}

/// Lets go of a slot's key and certificate (a session that left the epoch, a leg that ended).
pub fn forget(slot: &str) {
    HELD.lock().forget(|s| s == slot);
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_ci::identity::chain::{Actor, Identity};

    /// The answer kiln's CA gives `asked` for `identity`: a certificate for the CSR's key.
    fn answer(pki: &crate::pki::Pki, asked: &str, identity: &Identity) -> anyhow::Result<String> {
        let a: IdentityAsk = serde_json::from_str(asked)?;
        let pem = pki.issue_identity(&a.csr, identity, &[], time::Duration::hours(1))?;
        Ok(serde_json::to_string(&IdentityGiven { kind: a.kind, of: a.of, epoch: a.epoch, uri: identity.uri()?, cert_pem: Some(pem), error: None })?)
    }

    /// W27-R02: a slot's key is made here and only a CSR leaves; what it signs verifies under the
    /// certificate kiln issued for that CSR; let go of, it signs nothing more, its certificate's
    /// file goes, and a certificate coming for it afterwards is kept nowhere.
    #[test]
    fn w27_r02_a_key_stays_here_signs_and_goes() -> anyhow::Result<()> {
        use rcgen::PublicKeyData as _;
        let dir = tempfile::tempdir()?;
        let pki = crate::pki::Pki::open_scratch(&dir.path().join("pki"))?;
        let asked = ask("agent", Some("w27-r02-key"), Some(1))?;
        assert!(!asked.contains("PRIVATE"), "only a CSR leaves: {asked}");
        let identity = Identity::new(pki.organization(), Actor::Agent { person: "sam".into(), agent: "w27-r02-key".into(), epoch: 1, held_by: None });
        let given_json = answer(&pki, &asked, &identity)?;
        given(&given_json, dir.path(), "main")?;
        let held = slot("agent", Some("w27-r02-key"), Some(1));
        let file = dir.path().join("identities/main-agent_w27-r02-key_1.pem");
        assert!(file.exists(), "the certificate is written, public");
        let (sig, pem) = sign(&held, b"tree 4b825dc6\n")?;
        let v = kiln_ci::identity::chain::verify_signed(&[&crate::pki::der_of(&pem)?], &pki.ca_der, &[], crate::util::now_unix(), b"tree 4b825dc6\n", &sig)?;
        assert_eq!(v.identity, identity);
        // Asked again, the same key: the CSR is for the key the certificate names.
        let again: IdentityAsk = serde_json::from_str(&ask("agent", Some("w27-r02-key"), Some(1))?)?;
        let first: IdentityAsk = serde_json::from_str(&asked)?;
        let spki = |csr: &str| -> anyhow::Result<Vec<u8>> { Ok(rcgen::CertificateSigningRequestParams::from_pem(csr)?.public_key.der_bytes().to_vec()) };
        assert_eq!(spki(&again.csr)?, spki(&first.csr)?);
        forget(&held);
        assert!(sign(&held, b"x").is_err(), "a key let go of signs nothing");
        assert!(!file.exists(), "its certificate's file goes with it");
        given(&given_json, dir.path(), "main")?;
        assert!(cert(&held).is_none() && !file.exists(), "a certificate for a key let go of is kept nowhere");
        Ok(())
    }

    /// W27-R02: while a leg's key is held, this process isn't dumpable, so no process of its user
    /// (a leg's step) reaches its memory through ptrace or `/proc/<pid>/mem`, whatever Yama's ptrace
    /// scope; once the last leg's key goes, it's as it was. A runner's or an agent's key leaves it
    /// as it is.
    #[cfg(target_os = "linux")]
    #[test]
    fn w27_r02_memory_kept_from_steps_while_a_leg_is_held() -> anyhow::Result<()> {
        use rustix::process::{DumpableBehavior, dumpable_behavior};
        let before = dumpable_behavior()?;
        ask("agent", Some("w27-r02-mem"), Some(1))?;
        assert_eq!(dumpable_behavior()?, before, "an agent's key alone changes nothing");
        ask("leg", Some("w27-r02-a"), None)?;
        ask("leg", Some("w27-r02-b"), None)?;
        assert_eq!(dumpable_behavior()?, DumpableBehavior::NotDumpable);
        forget(&slot("leg", Some("w27-r02-a"), None));
        assert_eq!(dumpable_behavior()?, DumpableBehavior::NotDumpable, "another leg is still held");
        forget(&slot("leg", Some("w27-r02-b"), None));
        assert_eq!(dumpable_behavior()?, before, "the last leg gone, as it was");
        forget_prefix("agent/w27-r02-mem/");
        Ok(())
    }
}
