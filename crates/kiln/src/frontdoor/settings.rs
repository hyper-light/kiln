//! U17: accepted runner policy lives in the injected host machine Records namespace.
//! Certificate/key bytes remain physical files; settings.json is a bounded read-only import.
use std::path::{Path, PathBuf};
use std::sync::Arc;
use kiln_seams::Records;
use kiln_seams::records::{Cond, Consistency, Tx, Write};
use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use super::Settings;

const FAMILY: &str = "frontdoor-settings";
const MAX_BYTES: usize = 64 << 10;

#[derive(Serialize, Deserialize)]
struct Saved {
    version: u32,
    source: PathBuf,
    digest: String,
    settings: Settings,
}

pub(super) struct Policy {
    records: Arc<dyn Records>,
    key: Vec<u8>,
}

fn legacy(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    use std::io::Read as _;
    match std::fs::symlink_metadata(path) {
        Ok(meta) => anyhow::ensure!(meta.is_file(), "U17 front door settings source is not a regular file"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    anyhow::ensure!(file.metadata()?.is_file(), "U17 front door settings source is not a regular file");
    let mut bytes = Vec::new();
    file.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() <= MAX_BYTES, "U17 front door settings exceed 64 KiB");
    Ok(Some(bytes))
}

fn digest(bytes: Option<&[u8]>) -> String {
    bytes.map_or_else(|| "absent".into(), |b| format!("sha256:{}", hex::encode(sha2::Sha256::digest(b))))
}

fn decode(bytes: &[u8]) -> anyhow::Result<Saved> {
    anyhow::ensure!(bytes.len() <= MAX_BYTES, "U17 front door policy exceeds 64 KiB");
    let saved: Saved = serde_json::from_slice(bytes)?;
    anyhow::ensure!(saved.version == 1 && saved.source.is_absolute() && (saved.digest == "absent" || saved.digest.strip_prefix("sha256:").is_some_and(|d| d.len() == 64 && d.bytes().all(|b| b.is_ascii_hexdigit()))), "U17 unsupported or partial front door import receipt");
    anyhow::ensure!(saved.settings.clone().validated().map_err(anyhow::Error::msg)? == saved.settings, "U17 noncanonical front door settings");
    Ok(saved)
}

impl Policy {
    pub(super) async fn open(records: Arc<dyn Records>, dir: &Path) -> anyhow::Result<(Self, Settings)> {
        let path = std::path::absolute(dir)?.join("settings.json");
        let key = path.as_os_str().as_encoded_bytes().to_vec();
        let policy = Self { records, key };
        if let Some(row) = policy.records.get(FAMILY, &policy.key, Consistency::Linearizable).await? {
            return Ok((policy, decode(&row.value)?.settings));
        }
        let source = path.clone();
        let saved = crate::rt::blocking(move || -> anyhow::Result<Saved> {
            let bytes = legacy(&source)?;
            let settings = match bytes.as_deref() {
                Some(bytes) => {
                    // Deserialize the typed struct directly: duplicate policy fields are errors.
                    serde_json::from_slice::<Settings>(bytes)?.validated().map_err(anyhow::Error::msg)?
                }
                None => Settings::default(),
            };
            let saved = Saved { version: 1, source, digest: digest(bytes.as_deref()), settings };
            anyhow::ensure!(digest(legacy(&saved.source)?.as_deref()) == saved.digest, "U17 front door legacy settings changed during import");
            Ok(saved)
        }).await??;
        let value = serde_json::to_vec(&saved)?;
        anyhow::ensure!(value.len() <= MAX_BYTES, "U17 front door policy exceeds 64 KiB");
        let tx = Tx { conds: vec![Cond::Absent { family: FAMILY.into(), key: policy.key.clone() }], writes: vec![Write::Put { family: FAMILY.into(), key: policy.key.clone(), value }] };
        match policy.records.commit(tx).await {
            Ok(_) => Ok((policy, saved.settings)),
            // Another opener may have won. A complete canonical envelope is the only fallback.
            Err(kiln_seams::records::Refusal::Conflict { .. }) => {
                let row = policy.records.get(FAMILY, &policy.key, Consistency::Linearizable).await?.ok_or_else(|| anyhow::anyhow!("U17 front door import conflicted without canonical state"))?;
                Ok((policy, decode(&row.value)?.settings))
            }
            Err(e) => Err(e.into()),
        }
    }

    pub(super) async fn save(&self, settings: &Settings) -> anyhow::Result<()> {
        // Read first even after an uncertain prior outcome; never retry a speculative revision.
        let row = self.records.get(FAMILY, &self.key, Consistency::Linearizable).await?.ok_or_else(|| anyhow::anyhow!("U17 front door policy is missing"))?;
        let mut saved = decode(&row.value)?;
        saved.settings = settings.clone().validated().map_err(anyhow::Error::msg)?;
        let value = serde_json::to_vec(&saved)?;
        anyhow::ensure!(value.len() <= MAX_BYTES, "U17 front door policy exceeds 64 KiB");
        self.records.commit(Tx { conds: vec![Cond::At { family: FAMILY.into(), key: self.key.clone(), rev: row.rev }], writes: vec![Write::Put { family: FAMILY.into(), key: self.key.clone(), value }] }).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_seams::records::SimRecords;

    #[test]
    fn u17_frontdoor_import_is_read_only_and_restart_ignores_stale_legacy() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let db = dir.path().join("kiln.db");
            let front = dir.path().join("front-door");
            std::fs::create_dir(&front)?;
            let path = front.join("settings.json");
            let raw = br#"{"domain":"KILN.home"}"#;
            std::fs::write(&path, raw)?;
            let store = crate::store::Store::open(&db)?;
            let records: Arc<dyn Records> = Arc::new(crate::records::from_database(&store.database(), "machine.test")?);
            let (policy, imported) = Policy::open(records, &front).await?;
            assert_eq!(imported.domain, "kiln.home");
            assert_eq!(std::fs::read(&path)?, raw);
            let accepted = Settings { domain: "accepted.home".into(), ..Settings::default() };
            policy.save(&accepted).await?;
            assert_eq!(std::fs::read(&path)?, raw);
            drop(policy);
            drop(store);
            // Reopen the host owner and namespace, with an obsolete and now malformed source.
            std::fs::write(&path, b"obsolete source")?;
            let store = crate::store::Store::open(&db)?;
            let records: Arc<dyn Records> = Arc::new(crate::records::from_database(&store.database(), "machine.test")?);
            assert_eq!(Policy::open(records.clone(), &front).await?.1, accepted);
            // A distinct physical state root in this namespace cannot inherit this policy.
            assert_eq!(Policy::open(records, &dir.path().join("other")).await?.1, Settings::default());
            Ok(())
        })
    }

    #[test]
    fn u17_frontdoor_corrupt_canonical_policy_never_falls_back_to_legacy() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let records: Arc<dyn Records> = Arc::new(SimRecords::new());
            let (policy, _) = Policy::open(records.clone(), dir.path()).await?;
            records.commit(Tx { conds: Vec::new(), writes: vec![Write::Put { family: FAMILY.into(), key: policy.key.clone(), value: br#"{"version":2}"#.to_vec() }] }).await?;
            std::fs::write(dir.path().join("settings.json"), br#"{"domain":"stale.home"}"#)?;
            assert!(Policy::open(records, dir.path()).await.is_err());
            Ok(())
        })
    }

    #[test]
    fn u17_frontdoor_bad_legacy_refuses_without_import_receipt() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("settings.json");
            let records: Arc<dyn Records> = Arc::new(SimRecords::new());
            for raw in [b"{".to_vec(), br#"{"domain":"bad/name"}"#.to_vec(), br#"{"domain":"a.home","domain":"b.home"}"#.to_vec(), vec![b' '; MAX_BYTES + 1]] {
                std::fs::write(&path, &raw)?;
                assert!(Policy::open(records.clone(), dir.path()).await.is_err());
                assert_eq!(std::fs::read(&path)?, raw);
                assert_eq!(records.head().await?, 0);
            }
            std::fs::remove_file(&path)?;
            let (policy, settings) = Policy::open(records.clone(), dir.path()).await?;
            assert_eq!(settings, Settings::default());
            drop(policy);
            std::fs::write(&path, br#"{"domain":"resurrect.home"}"#)?;
            assert_eq!(Policy::open(records, dir.path()).await?.1, Settings::default());
            Ok(())
        })
    }
}
