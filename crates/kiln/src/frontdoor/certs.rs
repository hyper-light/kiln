//! What a runner keeps for its front door, under `<state>/front-door/`:
//!
//! | File | What | Mode |
//! | --- | --- | --- |
//! | `settings.json` | the settings last pushed, so the front door is back at once after a restart | 0644 |
//! | `kiln.key`, `kiln.pem` | the kiln CA's certificate in use and its key | 0600 |
//! | `kiln.key.pending` | the key of the CSR waiting for the CA | 0600 |
//! | `custom.key`, `custom.pem` | a certificate and key uploaded for the front door | 0600 |
//!
//! Keys are made here and never leave the machine, except an uploaded one, which arrives from the
//! server once and is never sent back.

use std::path::{Path, PathBuf};

use anyhow::Context as _;

use super::{CertInfo, CustomCert, Settings};

const SETTINGS: &str = "settings.json";
const KILN_CERT: &str = "kiln.pem";
const KILN_KEY: &str = "kiln.key";
const PENDING_KEY: &str = "kiln.key.pending";
const CUSTOM_CERT: &str = "custom.pem";
const CUSTOM_KEY: &str = "custom.key";

pub struct Store {
    dir: PathBuf,
}

fn read(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok().filter(|s| !s.trim().is_empty())
}

impl Store {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn load_settings(&self) -> Option<Settings> {
        let text = read(&self.dir.join(SETTINGS))?;
        serde_json::from_str::<Settings>(&text).ok().and_then(|s| s.validated().ok())
    }

    pub fn save_settings(&self, s: &Settings) -> anyhow::Result<()> {
        let json = serde_json::to_vec_pretty(s)?;
        crate::util::write_secret(&self.dir.join(SETTINGS), &json, 0o644).context("store the front door's settings")
    }

    /// The kiln CA's certificate in use and its key.
    pub fn kiln(&self) -> Option<(String, String)> {
        Some((read(&self.dir.join(KILN_CERT))?, read(&self.dir.join(KILN_KEY))?))
    }

    /// A CSR for `want`, signed with the pending key (made now when there is none, so a CSR
    /// asked again after a restart is for the same key).
    pub fn csr(&self, want: &[String]) -> anyhow::Result<String> {
        let path = self.dir.join(PENDING_KEY);
        let key = match read(&path) {
            Some(k) => k,
            None => {
                let k = crate::pki::new_key_pem()?;
                crate::util::write_secret(&path, k.as_bytes(), 0o600).context("store the front door's new key")?;
                k
            }
        };
        crate::pki::front_door_csr(want, &key)
    }

    /// Drop the pending key (renewing now): the next CSR is for a new one.
    pub fn forget_pending(&self) -> std::io::Result<()> {
        match std::fs::remove_file(self.dir.join(PENDING_KEY)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    /// The CA's certificate for the pending key: kept, and the pending key becomes the key in
    /// use. A certificate for any other key is refused (one already in use is fine).
    pub fn take_issued(&self, cert_pem: &str) -> anyhow::Result<()> {
        if self.kiln().is_some_and(|(c, _)| c.trim() == cert_pem.trim()) {
            return Ok(());
        }
        let pending = read(&self.dir.join(PENDING_KEY)).context("no key is waiting for a certificate")?;
        anyhow::ensure!(crate::pki::cert_matches_key(cert_pem, &pending), "the certificate is for another key");
        crate::util::write_secret(&self.dir.join(KILN_KEY), pending.as_bytes(), 0o600)?;
        crate::util::write_secret(&self.dir.join(KILN_CERT), cert_pem.as_bytes(), 0o600)?;
        let _ = std::fs::remove_file(self.dir.join(PENDING_KEY));
        Ok(())
    }

    /// The uploaded certificate chain and key.
    pub fn custom(&self) -> Option<(String, String)> {
        Some((read(&self.dir.join(CUSTOM_CERT))?, read(&self.dir.join(CUSTOM_KEY))?))
    }

    /// Keep an uploaded certificate and key (both 0600), once they are checked to belong
    /// together: the chain's certificates alone in one file (a key pasted with them is dropped
    /// there), the key in the other. Errors never quote the key.
    pub fn store_custom(&self, c: &CustomCert) -> anyhow::Result<CertInfo> {
        crate::http::tls::ServerTls::from_pem(&c.cert_pem, &c.key_pem)?;
        let info = crate::pki::cert_info(&c.cert_pem)?;
        let chain = crate::pki::public_chain_pem(&c.cert_pem)?;
        crate::util::write_secret(&self.dir.join(CUSTOM_KEY), c.key_pem.as_bytes(), 0o600).context("store the key")?;
        crate::util::write_secret(&self.dir.join(CUSTOM_CERT), chain.as_bytes(), 0o600).context("store the certificate")?;
        Ok(info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issued_certificates_must_be_for_the_pending_key() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::new(dir.path().join("front-door"));
        let pki = crate::pki::Pki::open(&dir.path().join("pki"))?;
        assert!(store.kiln().is_none());
        let want = vec!["kiln.home".to_owned(), "*.kiln.home".to_owned()];
        let csr = store.csr(&want)?;
        // Asked again (a restart): the same key.
        let again = store.csr(&want)?;
        let pem = pki.sign_front_door(&csr, &want, 30)?;
        let pem_again = pki.sign_front_door(&again, &want, 30)?;
        store.take_issued(&pem)?;
        let (cert, key) = store.kiln().ok_or_else(|| anyhow::anyhow!("stored"))?;
        assert_eq!(cert, pem);
        assert!(crate::pki::cert_matches_key(&pem_again, &key), "both CSRs used one key");
        // The same one again is fine; one for a stranger's key is not.
        store.take_issued(&pem)?;
        let stranger = crate::pki::front_door_csr(&want, &crate::pki::new_key_pem()?)?;
        let other = pki.sign_front_door(&stranger, &want, 30)?;
        assert!(store.take_issued(&other).is_err());
        assert_eq!(store.kiln().map(|(c, _)| c), Some(pem));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(store.dir().join(KILN_KEY))?.permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        Ok(())
    }

    #[test]
    fn uploaded_certificates_are_checked_and_kept_private() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::new(dir.path().to_path_buf());
        let pki = crate::pki::Pki::open(&dir.path().join("pki"))?;
        let key = crate::pki::new_key_pem()?;
        let cert = pki.sign_front_door(&crate::pki::front_door_csr(&["kiln.home".to_owned()], &key)?, &["kiln.home".to_owned()], 30)?;
        let wrong = CustomCert { cert_pem: cert.clone(), key_pem: crate::pki::new_key_pem()? };
        let e = store.store_custom(&wrong).err().map(|e| format!("{e:#}")).unwrap_or_default();
        assert!(e.contains("not the certificate's") && !e.contains("PRIVATE"), "{e}");
        assert!(store.custom().is_none());
        let info = store.store_custom(&CustomCert { cert_pem: cert.clone(), key_pem: key.clone() })?;
        assert_eq!(info.names, ["kiln.home"]);
        assert_eq!(store.custom(), Some((crate::pki::public_chain_pem(&cert)?, key.clone())));
        // Pasted with its key in the certificate field: the certificate file holds no key.
        store.store_custom(&CustomCert { cert_pem: format!("{cert}{key}"), key_pem: key.clone() })?;
        assert!(store.custom().is_some_and(|(c, k)| !c.contains("PRIVATE KEY") && k == key));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            for f in [CUSTOM_KEY, CUSTOM_CERT] {
                assert_eq!(std::fs::metadata(dir.path().join(f))?.permissions().mode() & 0o777, 0o600, "{f}");
            }
        }
        let s = Settings { domain: "kiln.home".into(), ..Settings::default() };
        store.save_settings(&s)?;
        assert_eq!(store.load_settings(), Some(s));
        Ok(())
    }
}
