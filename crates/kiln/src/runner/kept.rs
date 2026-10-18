//! Legs' shells and kept legs (docs/ci.md W17-R01, R02).
//!
//! - **Shells.** Each running leg says what a terminal into it opens: a shell in its workspace
//!   with its environment, or `docker exec` into its container, and the values its output masks.
//! - **Kept legs.** A failed leg with `keep-on-failure` keeps its workspace and containers (its
//!   processes are stopped) until its keep runs out or it's released; "Extend" keeps it longer.
//!   Its shell stays, so a terminal opens into it as into a running leg. A kept leg forgets its
//!   shell if the runner restarts (its environment held secrets, and none are written to disk);
//!   its workspace is then swept once its keep runs out.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;

use super::docker::Containers;

/// What a terminal into a leg opens.
#[derive(Debug, Clone, Default)]
pub struct Shell {
    /// Where it starts, on the host.
    pub workdir: PathBuf,
    /// The leg's environment: the runner's variables and the job's.
    pub env: BTreeMap<String, String>,
    /// Values masked in its output.
    pub masks: Vec<String>,
    /// For a leg in a container: the program and arguments that open a shell in it
    /// (`docker exec -it …`), and the environment that program needs.
    pub exec: Option<(Vec<String>, Vec<(String, String)>)>,
}

struct Kept {
    until: i64,
    ws: PathBuf,
    containers: Option<Containers>,
}

#[derive(Default)]
struct Entry {
    shell: Shell,
    /// Containers a failed leg left running, until it's kept or let go.
    held: Option<Containers>,
    kept: Option<Kept>,
}

static LEGS: LazyLock<parking_lot::Mutex<HashMap<String, Entry>>> = LazyLock::new(Default::default);

/// The longest a leg is kept (and extended to at once).
pub const MAX_KEEP_MINUTES: u32 = 24 * 60;

/// A leg started: what a terminal into it opens.
pub fn running(job: &str, shell: Shell) {
    LEGS.lock().entry(job.to_owned()).or_default().shell = shell;
}

/// A running leg's containers came up: a terminal opens into its job container.
pub fn exec_into(job: &str, exec: (Vec<String>, Vec<(String, String)>)) {
    if let Some(e) = LEGS.lock().get_mut(job) {
        e.shell.exec = Some(exec);
    }
}

/// What a terminal into `job` opens, while it runs or is kept.
pub fn shell(job: &str) -> Option<Shell> {
    LEGS.lock().get(job).map(|e| e.shell.clone())
}

/// Whether `job` runs or is kept here: a terminal into it stays open while it is.
pub fn live(job: &str) -> bool {
    LEGS.lock().contains_key(job)
}

/// A failed leg to be kept leaves its containers running here instead of stopping them.
pub fn hold(job: &str, containers: Containers) {
    if let Some(e) = LEGS.lock().get_mut(job) {
        e.held = Some(containers);
    }
}

/// The leg ended: kept for `keep_minutes` (a failed leg's `keep-on-failure`), returning until
/// when, or let go (its containers stopped, its workspace removed).
pub async fn ended(job: &str, ws: PathBuf, keep_minutes: Option<u32>) -> Option<i64> {
    let held = LEGS.lock().get_mut(job).and_then(|e| e.held.take());
    match keep_minutes.filter(|m| *m > 0) {
        Some(m) => {
            let until = crate::util::now_unix() + i64::from(m.min(MAX_KEEP_MINUTES)) * 60;
            super::workspaces::keep_until(&ws, until);
            LEGS.lock().entry(job.to_owned()).or_default().kept = Some(Kept { until, ws, containers: held });
            expire_at(job, until);
            Some(until)
        }
        None => {
            LEGS.lock().remove(job);
            super::runtime::unpark(job);
            if let Some(c) = held {
                c.stop().await;
            }
            super::workspaces::release(&ws).await;
            None
        }
    }
}

/// Lets a kept leg go at `until`, unless it was kept longer meanwhile.
fn expire_at(job: &str, until: i64) {
    let job = job.to_owned();
    crate::rt::spawn(async move {
        let wait = u64::try_from(until - crate::util::now_unix()).unwrap_or(0);
        crate::rt::sleep(Duration::from_secs(wait)).await;
        let due = LEGS.lock().get(&job).and_then(|e| e.kept.as_ref()).is_some_and(|k| k.until <= crate::util::now_unix());
        if due {
            release(&job).await;
        }
    });
}

/// A kept leg kept `minutes` from now; until when, if it is kept here.
pub fn extend(job: &str, minutes: u32) -> Option<i64> {
    let until = crate::util::now_unix() + i64::from(minutes.min(MAX_KEEP_MINUTES)) * 60;
    let ws = {
        let mut g = LEGS.lock();
        let k = g.get_mut(job)?.kept.as_mut()?;
        k.until = until;
        k.ws.clone()
    };
    super::workspaces::keep_until(&ws, until);
    expire_at(job, until);
    Some(until)
}

/// A kept leg let go now: its containers stopped, its workspace removed. Whether it was kept.
pub async fn release(job: &str) -> bool {
    super::runtime::unpark(job);
    let Some(e) = LEGS.lock().remove(job) else { return false };
    let Some(k) = e.kept else { return false };
    if let Some(c) = k.containers {
        c.stop().await;
    }
    super::workspaces::release(&k.ws).await;
    true
}

/// When `job` is kept until, if it is.
pub fn kept_until(job: &str) -> Option<i64> {
    LEGS.lock().get(job).and_then(|e| e.kept.as_ref()).map(|k| k.until)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w17_r02_kept_extended_released() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let ws = dir.path().join("w").join("aaaa0001");
            std::fs::create_dir_all(ws.join("repo"))?;
            running("leg-kept", Shell { workdir: ws.join("repo"), ..Shell::default() });
            assert!(shell("leg-kept").is_some(), "a running leg's shell");
            let until = ended("leg-kept", ws.clone(), Some(5)).await.ok_or_else(|| anyhow::anyhow!("kept"))?;
            assert!(until > crate::util::now_unix() + 290);
            assert_eq!(super::super::workspaces::kept_until(&ws), Some(until));
            assert!(ws.exists() && shell("leg-kept").is_some(), "kept: workspace and shell");
            let later = extend("leg-kept", 30).ok_or_else(|| anyhow::anyhow!("extended"))?;
            assert!(later > until && kept_until("leg-kept") == Some(later));
            assert!(release("leg-kept").await && !ws.exists() && shell("leg-kept").is_none() && !live("leg-kept"), "released");
            assert!(!release("leg-kept").await, "once");
            // A leg that isn't kept goes at once.
            let ws2 = dir.path().join("w").join("aaaa0002");
            std::fs::create_dir_all(&ws2)?;
            running("leg-done", Shell::default());
            assert_eq!(ended("leg-done", ws2.clone(), None).await, None);
            assert!(!ws2.exists() && !live("leg-done"));
            Ok(())
        })
    }
}
