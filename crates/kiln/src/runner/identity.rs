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

use std::collections::{HashMap, HashSet};
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
    /// What kiln said of each slot as it certified it: its identity's URI and, a leg's, the
    /// server's delegation of the leg by its digest, which its records act under.
    given: HashMap<String, Given>,
    /// The slots kiln said why it doesn't certify, as it answered them: nothing waits for them until
    /// they're asked for again.
    refused: HashSet<String>,
}

struct Given {
    uri: String,
    under: Option<String>,
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
        self.given.retain(|s, _| !which(s));
        self.refused.retain(|s| !which(s));
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
    Ok(serde_json::to_string(&request(kind, of, epoch)?)?)
}

/// [`ask`], before it's sent: what an agent session's ask carries beside its CSR is added to it
/// (`crate::runner::custody::carrying`).
pub fn request(kind: &str, of: Option<&str>, epoch: Option<u64>) -> anyhow::Result<IdentityAsk> {
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
        // Asked again (a refused agent session's on the heartbeat): awaited until kiln answers.
        held.refused.remove(&slot);
        let key = held.keys.get(&slot).ok_or_else(|| anyhow::anyhow!("no key for {slot}"))?;
        rcgen::CertificateParams::new(vec!["kiln-identity".to_owned()])?.serialize_request(key)?.pem()?
    };
    // A leg this runner runs signs its records, and the provenance kiln writes when asked
    // (docs/ci.md W27-R05, R06).
    Ok(IdentityAsk { kind: kind.to_owned(), csr, of: of.map(str::to_owned), epoch, signs: kind == LEG, ..IdentityAsk::default() })
}

/// The server's answer: the certificate kept for its slot (and written, public, under
/// `<state>/identities/<file>.pem`, `file` named by `label`), or why there's none, logged. One for
/// a key let go of meanwhile (a leg that ended first) is kept nowhere.
pub fn given(json: &str, state_dir: &Path, label: &str) -> anyhow::Result<()> {
    let g: IdentityGiven = serde_json::from_str(json)?;
    let slot = slot(&g.kind, g.of.as_deref(), g.epoch);
    let Some(pem) = g.cert_pem else {
        tracing::warn!(identity = %slot, error = %g.error.unwrap_or_default(), "kiln issued no identity");
        // Answered all the same: what waits for it goes on without it (a leg's first step, W27-R05).
        let mut held = HELD.lock();
        if held.keys.contains_key(&slot) {
            held.refused.insert(slot);
        }
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
    held.given.insert(slot.clone(), Given { uri: g.uri, under: g.delegation });
    held.refused.remove(&slot);
    held.certs.insert(slot, (pem, path));
    Ok(())
}

/// Whether a key is held under `slot` that kiln hasn't answered yet: asked for, and since neither
/// certified nor refused. What a leg's first step waits on (docs/ci.md W27-R05).
pub fn awaited(slot: &str) -> bool {
    let held = HELD.lock();
    held.keys.contains_key(slot) && !held.given.contains_key(slot) && !held.refused.contains(slot)
}

/// Whether this runner's server issues identities (`CAP_IDENTITY`), so its sessions' git signs as
/// their agents (W27-R03, [`crate::gitsign`]): a server from before keeps them committing unsigned.
static ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Says whether the server issues identities, as it said on connecting.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Whether the server issues identities ([`set_enabled`]).
pub fn enabled() -> bool {
    ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// The kiln server this runner works for, by the URLs it's reached at (the one this runner uses,
/// then those it says it's at), when that server gives agents their own git credential
/// (`CAP_AGENT_GIT`): its sessions' git pushes there as the agent (W27-R03,
/// [`crate::gitsign::git_env`]).
static KILN_GIT: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Says where the server is (none: it gives agents no git credential of their own), as it said on
/// connecting.
pub fn set_kiln_git(urls: Vec<String>) {
    KILN_GIT.lock().clear();
    also_kiln_git(urls);
}

/// Where else the server says it's reached (its public URL, its front doors).
pub fn also_kiln_git(urls: Vec<String>) {
    let mut held = KILN_GIT.lock();
    for u in urls.into_iter().map(|u| u.trim().trim_end_matches('/').to_owned()).filter(|u| u.contains("://")) {
        if !held.contains(&u) {
            held.push(u);
        }
    }
}

/// The URLs of the server whose git credential an agent session takes from it ([`set_kiln_git`]).
pub fn kiln_git() -> Vec<String> {
    KILN_GIT.lock().clone()
}

/// The newest epoch of agent session `session` this process holds a certified key at: the epoch
/// it holds the session at, as kiln certified it.
pub fn epoch_held(session: &str) -> Option<u64> {
    let prefix = format!("agent/{session}/");
    HELD.lock().certs.keys().filter_map(|k| k.strip_prefix(&prefix)?.parse::<u64>().ok()).max()
}

/// The certificate held for a slot.
pub fn cert(slot: &str) -> Option<String> {
    HELD.lock().certs.get(slot).map(|(pem, _)| pem.clone())
}

/// The URI of the identity certified for a slot.
pub fn uri(slot: &str) -> Option<String> {
    HELD.lock().given.get(slot).map(|g| g.uri.clone())
}

/// The delegation a slot's identity acts under, by its digest: a leg's, the server's delegation of
/// the leg, given with its certificate.
pub fn under(slot: &str) -> Option<String> {
    HELD.lock().given.get(slot).and_then(|g| g.under.clone())
}

/// Signs `message` with the slot's key before kiln has certified it (a receipt that goes with
/// the ask for its certificate): the signature. The key stays here.
pub fn sign_uncertified(slot: &str, message: &[u8]) -> anyhow::Result<Vec<u8>> {
    use rcgen::SigningKey as _;
    let held = HELD.lock();
    let key = held.keys.get(slot).ok_or_else(|| anyhow::anyhow!("no key for {slot}"))?;
    Ok(key.sign(message)?)
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
        Ok(serde_json::to_string(&IdentityGiven { kind: a.kind, of: a.of, epoch: a.epoch, uri: identity.uri()?, cert_pem: Some(pem), ..IdentityGiven::default() })?)
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

    /// W27-R05: a key asked for is awaited until kiln answers it, certifying it or saying why not,
    /// and again once it's asked for again: what a leg's first step waits on.
    #[test]
    fn w27_r05_a_key_is_awaited_from_its_ask_until_kiln_answers_it() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pki = crate::pki::Pki::open_scratch(&dir.path().join("pki"))?;
        let s = slot("agent", Some("w27-r05-wait"), Some(1));
        assert!(!awaited(&s), "nothing asked");
        ask("agent", Some("w27-r05-wait"), Some(1))?;
        assert!(awaited(&s), "asked, not answered");
        let refused = serde_json::to_string(&IdentityGiven { kind: "agent".into(), of: Some("w27-r05-wait".into()), epoch: Some(1), error: Some("not now".into()), ..IdentityGiven::default() })?;
        given(&refused, dir.path(), "main")?;
        assert!(!awaited(&s) && uri(&s).is_none(), "refused: answered, with no certificate");
        let asked = ask("agent", Some("w27-r05-wait"), Some(1))?;
        assert!(awaited(&s), "asked again");
        let identity = Identity::new(pki.organization(), Actor::Agent { person: "sam".into(), agent: "w27-r05-wait".into(), epoch: 1, held_by: None });
        given(&answer(&pki, &asked, &identity)?, dir.path(), "main")?;
        assert!(!awaited(&s) && uri(&s).is_some(), "certified");
        forget(&s);
        assert!(!awaited(&s), "let go of");
        given(&refused, dir.path(), "main")?;
        assert!(!HELD.lock().refused.contains(&s), "a refusal for a key let go of is kept nowhere");
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
