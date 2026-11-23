//! CP-T3.2. A former execution owner closes admission before attempting its inbox write.
//! The intent lives beside the host record, independently of the conversation database.
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, ensure};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Fence {
    pub session: String,
    pub old: u64,
    pub target: u64,
    pub confirmed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<(u32,u64)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resumed: Option<u64>,
    #[serde(default)]
    pub seq: u64,
    #[serde(default)]
    pub at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
}

impl Fence {
    fn precedes(&self, other: &Self) -> bool {
        (self.old, self.resumed, self.confirmed, self.target)
            < (other.old, other.resumed, other.confirmed, other.target)
    }
}

#[derive(Default)]
pub(crate) struct Fences {
    dir: Option<PathBuf>,
    pub changed: crate::rt::Changed,
    entries: RwLock<HashMap<String, Fence>>,
    load_error: RwLock<Option<String>>,
}

impl Fences {
    pub fn open(dir: Option<PathBuf>) -> Self {
        let Some(dir) = dir else {
            return Self::default();
        };
        match Self::read(&dir) {
            Ok(entries) => Self {
                dir: Some(dir),
                changed: Default::default(),
                entries: RwLock::new(entries),
                load_error: RwLock::new(None),
            },
            Err(e) => Self {
                dir: Some(dir),
                changed: Default::default(),
                entries: Default::default(),
                load_error: RwLock::new(Some(format!("{e:#}"))),
            },
        }
    }

    pub(crate) fn location(file: &Path) -> PathBuf {
        let own = file.parent();
        if let Some(instances) = own
            .and_then(Path::parent)
            .filter(|p| p.file_name().is_some_and(|n| n == "instances"))
            && let Some(state) = instances.parent()
        {
            return state.join("owner-fences");
        }
        file.with_extension("owner-fences")
    }

    /// A standby can run before the previous instance finishes fencing. Load its durable
    /// marks again when this process acquires the WAL. Never discard in-memory failed intents.
    pub fn rebind(&self) {
        let Some(dir) = &self.dir else { return };
        match Self::read(dir) {
            Ok(saved) => {
                let mut entries = self.entries.write();
                for (id, f) in saved {
                    if entries.get(&id).is_none_or(|old| !f.precedes(old)) {
                        entries.insert(id, f);
                    }
                }
                *self.load_error.write() = None;
            }
            Err(e) => *self.load_error.write() = Some(format!("{e:#}")),
        }
    }

    fn read(dir: &Path) -> Result<HashMap<String, Fence>> {
        let entries = match std::fs::read_dir(dir) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = HashMap::new();
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let fence: Fence =
                serde_json::from_reader(std::io::BufReader::new(std::fs::File::open(&path)?))?;
            ensure!(
                path == Self::path(dir, &fence.session)
                    && fence.target >= fence.old
                    && fence.provider.is_none_or(|(pid,born)| pid > 1 && born > 0)
                    && fence
                        .resumed
                        .is_none_or(|e| fence.confirmed && e > fence.old && e >= fence.target),
                "invalid execution owner fence"
            );
            out.insert(fence.session.clone(), fence);
        }
        Ok(out)
    }

    fn path(dir: &Path, id: &str) -> PathBuf {
        dir.join(format!("{}.json", crate::util::sha256_hex(id.as_bytes())))
    }

    pub fn check(&self, id: &str) -> Result<()> {
        self.check_epoch(id, None)
    }

    pub fn check_epoch(&self, id: &str, epoch: Option<u64>) -> Result<()> {
        if let Some(cause) = self.load_error.read().as_deref() {
            anyhow::bail!("execution owner fences unavailable: {cause}");
        }
        let entries = self.entries.read();
        let Some(f) = entries.get(id) else { return Ok(()) };
        if let Some(resumed) = f.resumed {
            ensure!(epoch.is_none_or(|n| n >= resumed), "the local execution owner resumed at epoch {resumed}");
            return Ok(());
        }
        anyhow::bail!(
            "the old owner at epoch {} is fenced through epoch {}{}",
            f.old, f.target,
            if f.confirmed { "" } else { " pending durable confirmation" }
        );
    }

    pub fn get(&self, id: &str) -> Option<Fence> {
        self.entries
            .read()
            .get(id)
            .filter(|f| f.resumed.is_none())
            .cloned()
    }

    pub fn notice(&self, id: &str) -> Option<crate::harness::model::Event> {
        let f = self.entries.read().get(id)?.clone();
        Some(crate::harness::model::Event {
            id: format!("owner-fence-{}", f.old),
            seq: f.seq,
            at: f.at,
            parent: None,
            by: Some("kiln".into()),
            body: crate::harness::model::Body::Notice {
                level: crate::harness::model::Level::Warn,
                text: format!(
                    "Inbox fencing failed for epoch {} while advancing to {}. The old owner refuses new messages. {}",
                    f.old, f.target, f.cause?
                ),
            },
        })
    }

    pub fn pending(&self) -> Vec<String> {
        self.entries
            .read()
            .values()
            .filter(|f| !f.confirmed)
            .map(|f| f.session.clone())
            .collect()
    }

    pub fn begin(&self, id: &str, old: u64, target: u64) -> Fence {
        let mut entries = self.entries.write();
        let f = entries.entry(id.to_owned()).or_insert_with(|| Fence {
            session: id.to_owned(),
            old,
            target,
            ..Default::default()
        });
        if old > f.old {
            f.old = old;
            f.confirmed = false;
            f.provider = None;
            f.cause = None;
            f.resumed = None;
            f.at = 0;
            f.seq = 0;
        }
        f.target = f.target.max(target);
        f.clone()
    }

    // The host serializes the entire fence/activation transition per session.
    pub fn show(&self, fence: &Fence) {
        let mut entries = self.entries.write();
        if entries
            .get(&fence.session)
            .is_none_or(|current| !fence.precedes(current))
        {
            entries.insert(fence.session.clone(), fence.clone());
            drop(entries);
            self.changed.notify();
        }
    }

    pub fn save(&self, fence: &Fence) -> Result<()> {
        let mut fence = fence.clone();
        if self.load_error.read().is_some() {
            self.rebind();
        }
        if let Some(cause) = self.load_error.read().as_deref() {
            anyhow::bail!("execution owner fence directory unavailable: {cause}");
        }
        if let Some(dir) = &self.dir {
            std::fs::create_dir_all(dir)?;
            crate::blobs::chunks::sync_dir(
                dir.parent().context("missing fence directory parent")?,
            )?;
            let path = Self::path(dir, &fence.session);
            // A blue/green predecessor can have a write on a blocking worker while its
            // successor acquires the WAL. Serialize their marks and retain the greater epoch.
            let lock = crate::os::with_mode(
                std::fs::OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .write(true),
                0o600,
            )
            .open(path.with_extension("lock"))?;
            lock.lock()?;
            match std::fs::File::open(&path) {
                Ok(file) => {
                    let current: Fence = serde_json::from_reader(std::io::BufReader::new(file))?;
                    ensure!(
                        current.session == fence.session,
                        "execution owner fence names a different session"
                    );
                    if fence.precedes(&current) {
                        self.show(&current);
                        anyhow::bail!(
                            "a newer epoch or completed execution owner fence superseded this attempt"
                        );
                    }
                    if fence.old == current.old && fence.provider.is_none() { fence.provider = current.provider; }
                    if fence.old == current.old && fence.cause.is_none() && current.cause.is_some()
                    {
                        // Preserve a failure already recorded by the other blue/green process.
                        fence.cause = current.cause;
                        fence.at = current.at;
                        fence.seq = current.seq;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            crate::util::write_secret(&path, &serde_json::to_vec(&fence)?, 0o600)?;
            crate::blobs::chunks::sync_dir(dir)?;
        }
        self.show(&fence);
        Ok(())
    }

    pub fn activate(&self, id: &str, epoch: u64) -> Result<()> {
        let Some(mut f) = self.get(id) else {
            return self.check_epoch(id, Some(epoch));
        };
        if !f.confirmed || epoch <= f.old || epoch < f.target {
            return Err(Pending(format!(
                "the prior epoch's durable fence has not cleared for {id}"
            ))
            .into());
        }
        // The higher epoch is a small durable floor. It also prevents an old blocking
        // writer from reinstalling this fence after a blue/green handover.
        f.resumed = Some(epoch);
        self.save(&f)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct Pending(pub String);

/// Attempts are released by a new ownership decision or a write the store actually confirmed.
#[derive(Default)]
pub(crate) struct RetryWaits {
    parked: HashMap<String, u64>,
}
impl RetryWaits {
    pub fn is_empty(&self) -> bool {
        self.parked.is_empty()
    }
    pub fn ownership(&mut self, id: &str) {
        self.parked.remove(id);
    }
    pub fn park(&mut self, id: String, at: u64) {
        self.parked.insert(id, at);
    }
    pub fn writable(&mut self, at: u64, mut retry: impl FnMut(String)) {
        self.parked.retain(|id, prior| {
            if *prior == at {
                true
            } else {
                retry(id.clone());
                false
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cp_t3_2_a_late_predecessor_cannot_refence_a_reactivated_owner() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let original = Fences::open(Some(dir.path().to_path_buf()));
        let mut stale = original.begin("session", 1, 2);
        original.save(&stale)?;
        let successor = Fences::open(Some(dir.path().to_path_buf()));
        let mut confirmed = successor.get("session").context("missing intent")?;
        stale.cause = Some("disk unavailable".into());
        original.save(&stale)?;
        confirmed.confirmed = true;
        successor.save(&confirmed)?;
        successor.activate("session", 2)?;
        ensure!(original.save(&stale).is_err());
        original.rebind();
        original.check("session")?;
        ensure!(original.activate("session", 1).is_err());
        original.activate("session", 2)?;
        ensure!(original.notice("session").is_some());
        Ok(())
    }
}
