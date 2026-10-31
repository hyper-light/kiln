//! U17: versioned control snapshots in the host's shared Records namespace.
use std::{path::Path, sync::Arc};
use kiln_seams::records::{Cond, Consistency, Records, Rev, Tx, Write, KEY_MAX, VALUE_MAX};
use parking_lot::Mutex;
use serde::{Serialize, de::DeserializeOwned};

const FAMILY: &str = "session-control";

/// A single writer's snapshot. CAS fences stale blue/green processes; conflicts never retry
/// an old in-memory snapshot over the new owner's state. The parent supplies the sole backend.
pub struct Snapshot {
    records: Arc<dyn Records>,
    key: Vec<u8>,
    revision: Mutex<Rev>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Envelope<T> { version: u32, state: T }

#[derive(serde::Serialize, serde::Deserialize)]
struct Segments { version: u32, bytes: usize, segments: usize, digest: String }
const SNAPSHOT_MAX: usize = 64 << 20;

impl Snapshot {
    pub fn scope(&self) -> String { String::from_utf8_lossy(&self.key).into_owned() }
    pub(crate) fn records(&self) -> Arc<dyn Records> { Arc::clone(&self.records) }

    pub(crate) fn peek<T: DeserializeOwned>(records: Arc<dyn Records>, scope: &str) -> anyhow::Result<Option<T>> {
        anyhow::ensure!(!scope.is_empty() && !scope.contains('\0') && scope.len() <= KEY_MAX.saturating_sub(16), "U17 invalid session snapshot scope");
        Self { records, key: scope.as_bytes().to_vec(), revision: Mutex::new(0) }.read()
    }

    /// Import only if absent. Keeping an empty snapshot also marks an absent legacy input as
    /// imported, so a forgotten JSON copy can never become authoritative on a later restart.
    pub fn open<T: Serialize + DeserializeOwned + Default>(records: Arc<dyn Records>, scope: &str, legacy: Option<&Path>) -> anyhow::Result<(Self, T)> {
        Self::import(records, scope, || match legacy.map(std::fs::read) {
            Some(Ok(raw)) => serde_json::from_slice(&raw).map_err(|e| anyhow::anyhow!("U17 legacy session state is unreadable: {e}")),
            Some(Err(e)) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(T::default()),
        })
    }

    pub fn import<T: Serialize + DeserializeOwned>(records: Arc<dyn Records>, scope: &str, legacy: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<(Self, T)> {
        anyhow::ensure!(!scope.is_empty() && !scope.contains('\0') && scope.len() <= KEY_MAX.saturating_sub(16), "U17 invalid session snapshot scope");
        let snapshot = Self { records, key: scope.as_bytes().to_vec(), revision: Mutex::new(0) };
        if let Some(state) = snapshot.read()? { return Ok((snapshot, state)); }
        let state = legacy()?;
        match snapshot.save(&state) {
            Ok(()) => Ok((snapshot, state)),
            Err(e) => {
                // A concurrent importer may have committed first. Read its complete snapshot;
                // all other failures remain visible, without falling back to legacy state.
                if let Some(state) = snapshot.read()? { Ok((snapshot, state)) } else { Err(e) }
            }
        }
    }

    pub fn read<T: DeserializeOwned>(&self) -> anyhow::Result<Option<T>> {
        let mut revision = self.revision.lock();
        let Some((at, state)) = self.read_versioned()? else { return Ok(None) };
        *revision = at;
        Ok(Some(state))
    }

    /// U17: bind a read/modify/write to the revision returned with that state. Other readers
    /// must not re-arm its CAS by refreshing a shared cached revision in the meantime.
    pub fn read_versioned<T: DeserializeOwned>(&self) -> anyhow::Result<Option<(Rev, T)>> {
        let row = futures::executor::block_on(self.records.get(FAMILY, &self.key, Consistency::Linearizable))?;
        let Some(row) = row else { return Ok(None) };
        let header: serde_json::Value = serde_json::from_slice(&row.value)?;
        let version = header.get("version").and_then(serde_json::Value::as_u64);
        let raw = if version == Some(2) {
            let manifest: Segments = serde_json::from_slice(&row.value)?;
            anyhow::ensure!(manifest.bytes <= SNAPSHOT_MAX && manifest.segments == manifest.bytes.div_ceil(VALUE_MAX), "U17 invalid segmented snapshot bounds");
            let mut raw = Vec::with_capacity(manifest.bytes);
            for n in 0..manifest.segments {
                let key = self.segment_key(n);
                let segment = futures::executor::block_on(self.records.get(FAMILY, &key, Consistency::Snapshot(row.rev)))?
                    .ok_or_else(|| anyhow::anyhow!("U17 snapshot segment {n} missing at committed revision {}", row.rev))?;
                anyhow::ensure!(segment.rev == row.rev, "U17 snapshot segment revision mismatch");
                raw.extend_from_slice(&segment.value);
                anyhow::ensure!(raw.len() <= manifest.bytes, "U17 snapshot segment exceeds declared size");
            }
            anyhow::ensure!(raw.len() == manifest.bytes && crate::util::sha256_hex(&raw) == manifest.digest, "U17 snapshot segments corrupt");
            raw
        } else {
            anyhow::ensure!(version == Some(1), "U17 unsupported session snapshot version {version:?}");
            row.value
        };
        let envelope: Envelope<T> = serde_json::from_slice(&raw)?;
        anyhow::ensure!(envelope.version == 1, "U17 unsupported session snapshot payload version");
        Ok(Some((row.rev, envelope.state)))
    }

    fn segment_key(&self, n: usize) -> Vec<u8> {
        let mut key = self.key.clone();
        key.extend_from_slice(format!("\0{n:08}").as_bytes());
        key
    }

    pub fn save<T: Serialize>(&self, state: &T) -> anyhow::Result<()> {
        self.update(state, |tx| {
            let rev = futures::executor::block_on(self.records.commit(tx))?;
            Ok((Some(rev), ()))
        })
    }

    /// U17: acceptance can add its inbox writes to this transaction. The revision moves only
    /// when the caller actually committed, never when the inbox returned a duplicate's answer.
    pub fn update<T: Serialize, R>(&self, state: &T, commit: impl FnOnce(Tx) -> anyhow::Result<(Option<Rev>, R)>) -> anyhow::Result<R> {
        let mut revision = self.revision.lock();
        self.commit_state(state, *revision, commit, &mut revision)
    }

    pub fn save_at<T: Serialize>(&self, state: &T, expected: Rev) -> anyhow::Result<()> {
        let mut revision = self.revision.lock();
        self.commit_state(state, expected, |tx| {
            let rev = futures::executor::block_on(self.records.commit(tx))?;
            Ok((Some(rev), ()))
        }, &mut revision)
    }

    fn commit_state<T: Serialize, R>(&self, state: &T, expected: Rev, commit: impl FnOnce(Tx) -> anyhow::Result<(Option<Rev>, R)>, revision: &mut Rev) -> anyhow::Result<R> {
        let family = FAMILY.to_owned();
        let key = self.key.clone();
        let condition = if expected == 0 { Cond::Absent { family: family.clone(), key: key.clone() } } else { Cond::At { family: family.clone(), key: key.clone(), rev: expected } };
        let raw = serde_json::to_vec(&Envelope { version: 1, state })?;
        anyhow::ensure!(raw.len() <= SNAPSHOT_MAX, "U17 session snapshot exceeds {SNAPSHOT_MAX} bytes");
        let mut writes = Vec::new();
        let value = if raw.len() > VALUE_MAX {
            for (n, chunk) in raw.chunks(VALUE_MAX).enumerate() {
                writes.push(Write::Put { family: family.clone(), key: self.segment_key(n), value: chunk.to_vec() });
            }
            serde_json::to_vec(&Segments { version: 2, bytes: raw.len(), segments: raw.len().div_ceil(VALUE_MAX), digest: crate::util::sha256_hex(&raw) })?
        } else { raw };
        writes.push(Write::Put { family, key, value });
        let (committed, result) = commit(Tx { conds: vec![condition], writes })?;
        if let Some(rev) = committed { *revision = rev; }
        Ok(result)
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn u17_restart_ignores_legacy_after_committed_import_and_fences_stale_writer() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let legacy = dir.path().join("local.json");
        std::fs::write(&legacy, b"[1]")?;
        let records: Arc<dyn Records> = Arc::new(kiln_seams::records::SimRecords::default());
        let (first, imported) = Snapshot::open::<Vec<u32>>(records.clone(), "host/main", Some(&legacy))?;
        assert_eq!(imported, vec![1]);
        let (second, _) = Snapshot::open::<Vec<u32>>(records.clone(), "host/main", Some(&legacy))?;
        second.save(&vec![2, 3])?;
        assert!(first.save(&vec![4]).is_err());
        std::fs::write(&legacy, b"broken old copy")?;
        let (_, restored) = Snapshot::open::<Vec<u32>>(records, "host/main", Some(&legacy))?;
        assert_eq!(restored, vec![2, 3]);
        Ok(())
    }
    #[test]
    fn u17_interrupted_invalid_import_can_resume_without_empty_commit() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let legacy = dir.path().join("harness.json");
        let records: Arc<dyn Records> = Arc::new(kiln_seams::records::SimRecords::default());
        std::fs::write(&legacy, b"[")?;
        assert!(Snapshot::open::<Vec<u32>>(records.clone(), "runner/main", Some(&legacy)).is_err());
        std::fs::write(&legacy, b"[7]")?;
        let (_, state) = Snapshot::open::<Vec<u32>>(records, "runner/main", Some(&legacy))?;
        assert_eq!(state, vec![7]);
        Ok(())
    }
    #[test]
    fn u17_segmented_snapshot_reopens_above_one_mib_and_refuses_stale_atomic_replacement() -> anyhow::Result<()> {
        let records: Arc<dyn Records> = Arc::new(kiln_seams::records::SimRecords::default());
        let (first, _) = Snapshot::open::<Vec<String>>(records.clone(), "large", None)?;
        let (stale, _) = Snapshot::open::<Vec<String>>(records.clone(), "large", None)?;
        let large = vec!["q".repeat(VALUE_MAX), "r".repeat(VALUE_MAX)];
        first.save(&large)?;
        assert!(stale.save(&vec!["obsolete".to_owned()]).is_err());
        let (reopened, state) = Snapshot::open::<Vec<String>>(records.clone(), "large", None)?;
        assert_eq!(state, large);
        reopened.save(&vec!["small".to_owned()])?;
        let (_, state) = Snapshot::open::<Vec<String>>(records, "large", None)?;
        assert_eq!(state, vec!["small"]);
        Ok(())
    }

    #[test]
    fn u17_inbox_ack_and_replay_payload_commit_together_and_cas_failure_accepts_nothing() -> anyhow::Result<()> {
        use crate::harness::inbox::{Inbox, Limits};
        let records: Arc<dyn Records> = Arc::new(kiln_seams::records::SimRecords::default());
        let inbox = Inbox::new(records.clone(), Limits::default());
        futures::executor::block_on(inbox.open("s", 1))?;
        let (snapshot, _) = Snapshot::open::<Vec<String>>(records.clone(), "ack", None)?;
        let (stale, _) = Snapshot::open::<Vec<String>>(records.clone(), "ack", None)?;
        let accepted = snapshot.update(&vec!["queued words"], |tx| {
            let answer = futures::executor::block_on(inbox.accept_with("s", "m", "digest", Some(1), 1, tx))?;
            Ok((Some(answer.seq), answer))
        })?;
        assert!(!accepted.repeat);
        let (_, recovered) = Snapshot::open::<Vec<String>>(records.clone(), "ack", None)?;
        assert_eq!(recovered, vec!["queued words"]);
        let refused = stale.update(&vec!["lost words"], |tx| {
            let answer = futures::executor::block_on(inbox.accept_with("s", "m2", "digest2", Some(1), 2, tx))?;
            Ok((Some(answer.seq), answer))
        });
        assert!(refused.is_err());
        let retry = futures::executor::block_on(inbox.accept("s", "m2", "digest2", Some(1), 3))?;
        assert!(!retry.repeat, "refused snapshot must not leave a taken receipt");
        let repeat = futures::executor::block_on(inbox.accept("s", "m", "digest", Some(1), 4))?;
        assert!(repeat.repeat);
        let (_, recovered) = Snapshot::open::<Vec<String>>(records, "ack", None)?;
        assert_eq!(recovered, vec!["queued words"]);
        Ok(())
    }

    #[test]
    fn u17_machine_read_modify_write_cannot_borrow_a_newer_read_revision() -> anyhow::Result<()> {
        let records: Arc<dyn Records> = Arc::new(kiln_seams::records::SimRecords::default());
        let (machine, _) = Snapshot::open::<Vec<String>>(records.clone(), "machine", None)?;
        let (other, _) = Snapshot::open::<Vec<String>>(records, "machine", None)?;
        let (revision, mut old) = machine.read_versioned::<Vec<String>>()?.ok_or_else(|| anyhow::anyhow!("no machine state"))?;
        old.push("old decision".into());
        other.save(&vec!["new decision".to_owned()])?;
        let _ = machine.read::<Vec<String>>()?; // An unrelated reader refreshes the cache.
        assert!(machine.save_at(&old, revision).is_err(), "old state must retain its own CAS revision");
        assert_eq!(machine.read::<Vec<String>>()?, Some(vec!["new decision".into()]));
        Ok(())
    }

}
