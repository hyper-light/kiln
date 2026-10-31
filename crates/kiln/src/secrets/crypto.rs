//! U09: the shared encryption and master-key lifecycle. Legacy AAD is retained on import.
use std::path::{Path, PathBuf};
use anyhow::Context as _;
use aws_lc_rs::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use aws_lc_rs::rand::{SecureRandom, SystemRandom};
use crate::hub::Hub;

mod bootstrap;
pub(crate) use bootstrap::initialize;

/// U09/U15: no Debug/Serialize; one validated key lives with its Hub, never with a SQL call.
#[derive(Clone)]
pub(crate) struct Master([u8; 32]);

impl Master {
    pub(crate) fn bytes(&self) -> [u8; 32] { self.0 }

    pub(crate) fn seal(&self, plain: &[u8], aad: &[u8]) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
        seal(&self.0, plain, aad)
    }

    pub(crate) fn open(&self, sealed: &[u8], nonce: &[u8], aad: &[u8]) -> anyhow::Result<Vec<u8>> {
        open(&self.0, sealed, nonce, aad)
    }

    /// Explicit fixture provisioning only; production consumers receive the Hub's accepted key.
    #[cfg(test)]
    pub(crate) fn from_existing(path: &Path) -> anyhow::Result<Self> {
        master_at_mode(path, false).map(Self)
    }
}

pub(crate) fn master_path(cfg: &crate::config::Config) -> PathBuf {
    cfg.ci.secrets.master_key_file.clone().unwrap_or_else(|| cfg.data("secrets").join("master.key"))
}

pub(crate) fn random<const N: usize>() -> anyhow::Result<[u8; N]> {
    let mut b = [0u8; N];
    SystemRandom::new().fill(&mut b).map_err(|_| anyhow::anyhow!("the system's random source failed"))?;
    Ok(b)
}

/// U09/U15: construction validates and caches this before any sessions or crypto can run.
/// Reading it never reenters Store, including when called from inside a Store closure.
pub(crate) fn master(hub: &Hub) -> anyhow::Result<[u8; 32]> {
    Ok(hub.master_key.bytes())
}

#[cfg(test)]
fn master_at(p: &Path) -> anyhow::Result<[u8; 32]> {
    master_at_mode(p, true)
}

fn master_parent(p: &Path) -> anyhow::Result<&Path> {
    let parent = p.parent().context("master key has no directory")?;
    Ok(if parent.as_os_str().is_empty() { Path::new(".") } else { parent })
}

fn master_at_mode(p: &Path, create: bool) -> anyhow::Result<[u8; 32]> {
    use std::io::Read as _;
    let parent = master_parent(p)?;
    std::fs::create_dir_all(parent)?;
    // U09: overlapping SDK frontends share ciphertext and must share the same bootstrap key.
    // Keep this physical lock only through read/create and durable publication, never a lease.
    let lock = crate::os::with_mode(std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false), 0o600)
        .open(parent.join(".master.key.lock"))?;
    lock.try_lock().map_err(|e| anyhow::anyhow!("master key is busy or unavailable: {e}"))?;
    let k = match std::fs::File::open(p) {
        Ok(mut file) => {
            let metadata = file.metadata()?;
            anyhow::ensure!(metadata.is_file() && metadata.len() == 32, "{} is not a regular 32-byte master key", p.display());
            let mut k = [0u8; 32];
            file.read_exact(&mut k)?;
            // A prior publisher may have failed after rename. Verify durability before use.
            #[cfg(unix)]
            file.sync_all()?;
            k
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && create => {
            let k: [u8; 32] = random()?;
            crate::util::write_secret(p, &k, 0o600)?;
            k
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => anyhow::bail!("bootstrap master key is missing at {}; provision the existing trusted 32-byte master before starting this Hub", p.display()),
        Err(e) => return Err(e.into()),
    };
    crate::blobs::chunks::sync_dir(parent)?;
    if parent.parent().is_some() { crate::blobs::chunks::sync_dir(master_parent(parent)?)?; }
    Ok(k)
}

/// A key of its own for `label`, from the master key: one kiln checks with and never stores (an
/// inbound webhook's secret, W03-R02).
pub(crate) fn derived(hub: &Hub, label: &str) -> anyhow::Result<String> {
    Ok(crate::util::hmac_hex(&master(hub)?, label.as_bytes()))
}

fn key(bytes: &[u8; 32]) -> anyhow::Result<LessSafeKey> {
    Ok(LessSafeKey::new(UnboundKey::new(&AES_256_GCM, bytes).map_err(|_| anyhow::anyhow!("bad key"))?))
}

/// Seals `plain` under `k`, bound to `aad`: `(ciphertext with tag, nonce)`.
pub(crate) fn seal(k: &[u8; 32], plain: &[u8], aad: &[u8]) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let nonce: [u8; NONCE_LEN] = random()?;
    let mut buf = plain.to_vec();
    key(k)?.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(aad), &mut buf).map_err(|_| anyhow::anyhow!("sealing failed"))?;
    Ok((buf, nonce.to_vec()))
}

pub(crate) fn open(k: &[u8; 32], sealed: &[u8], nonce: &[u8], aad: &[u8]) -> anyhow::Result<Vec<u8>> {
    let n: [u8; NONCE_LEN] = nonce.try_into().map_err(|_| anyhow::anyhow!("bad nonce"))?;
    let mut buf = sealed.to_vec();
    let len = key(k)?.open_in_place(Nonce::assume_unique_for_key(n), Aad::from(aad), &mut buf).map_err(|_| anyhow::anyhow!("a secret doesn't decrypt: the master key or the database changed"))?.len();
    buf.truncate(len);
    Ok(buf)
}

/// Seals `plain` under the master key, bound to `aad` (keys of other subsystems: OIDC, W10).
pub(crate) fn seal_master(hub: &Hub, plain: &[u8], aad: &[u8]) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    seal(&master(hub)?, plain, aad)
}

pub(crate) fn open_master(hub: &Hub, sealed: &[u8], nonce: &[u8], aad: &[u8]) -> anyhow::Result<Vec<u8>> {
    open(&master(hub)?, sealed, nonce, aad)
}

#[cfg(test)]
mod tests {
    #[test]
    fn u09_overlapping_master_publishers_keep_one_durable_key_and_refuse_corruption() -> anyhow::Result<()> {
        // U09/U15: bare relative overrides use the current directory for both publication and
        // its parent sync; no test changes the process-wide working directory.
        assert_eq!(super::master_parent(std::path::Path::new("provisioned-master"))?, std::path::Path::new("."));
        assert_eq!(super::master_parent(std::path::Path::new("."))?, std::path::Path::new("."));
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("secrets/master.key");
        let key = super::master_at(&path)?;
        let lock = std::fs::File::options().read(true).write(true).open(path.with_file_name(".master.key.lock"))?;
        lock.try_lock()?;
        assert!(super::master_at(&path).is_err(), "an overlapping publisher must not replace an existing key");
        drop(lock);
        assert_eq!(super::master_at(&path)?, key);
        let (ciphertext, nonce) = super::seal(&key, b"acknowledged secret", b"scope")?;
        assert_eq!(super::open(&super::master_at(&path)?, &ciphertext, &nonce, b"scope")?, b"acknowledged secret");
        std::fs::remove_file(&path)?;
        assert!(super::Master::from_existing(&path).is_err(), "an existing-key frontend must refuse a missing bootstrap key");
        assert!(!path.exists(), "a credential writer must not replace a missing bootstrap key");
        std::fs::write(&path, b"broken")?;
        assert!(super::master_at(&path).is_err());
        assert_eq!(std::fs::read(path)?, b"broken", "a corrupt bootstrap key must never be regenerated");
        Ok(())
    }

    #[test]
    fn u09_u15_hub_cached_master_works_inside_store_calls_after_file_change() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let before = super::derived(&hub, "u09 cached master")?;
            crate::rt::blocking(move || -> anyhow::Result<()> {
                let path = super::master_path(&hub.cfg);
                let dir = hub.cfg.data("observe");
                let accepted = hub.master_key.clone();
                let targets = crate::observe::scrape::Targets::open(&dir, accepted.clone());
                let mut target: crate::observe::scrape::Target = serde_json::from_value(serde_json::json!({ "url": "http://127.0.0.1:9100/metrics", "bearer": "credential-before" }))?;
                targets.put("cached", target.clone()).map_err(anyhow::Error::msg)?;
                std::fs::write(&path, [39u8; 32])?;
                let nested = hub.clone();
                let after = hub.store.now(move |_| super::derived(&nested, "u09 cached master"))?;
                assert_eq!(after, before, "derivation must use the cached key without entering Store again");
                target.bearer = Some("credential-after-replacement".into());
                targets.put("cached", target.clone()).map_err(anyhow::Error::msg)?;
                assert_eq!(crate::observe::scrape::Targets::open(&dir, accepted.clone()).get("cached"), Some(target.clone()), "live Observe writes and reads retain the Hub's accepted master");
                assert!(super::initialize(&hub.cfg, &hub.store).is_err(), "the next Hub must refuse the changed file");
                std::fs::remove_file(&path)?;
                target.bearer = Some("credential-after-removal".into());
                targets.put("cached", target.clone()).map_err(anyhow::Error::msg)?;
                assert_eq!(crate::observe::scrape::Targets::open(&dir, accepted).get("cached"), Some(target), "a missing file cannot change the live Observe key");
                assert!(super::initialize(&hub.cfg, &hub.store).is_err(), "the next Hub must refuse the missing file");
                assert!(!path.exists(), "Observe must not recreate the accepted master file");
                hub.shutdown.cancel();
                Ok(())
            }).await?
        })
    }
}

