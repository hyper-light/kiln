//! Legs' workspaces (docs/ci.md W06-R01, W17-R02). Each leg's `<work>/w/<8 hex>` is removed when
//! the leg ends, unless it failed under `keep-on-failure`, which keeps it that long: a
//! `.kiln-keep-until` file in it says until when. Each leg's start also sweeps what is left
//! behind: workspaces no running leg uses whose keep has run out, or that never had one (a runner
//! stopped mid-leg). The tool cache, actions and Node runtimes beside them stay (W06-R01).
//!
//! A running leg holds a lock beside its workspace (`<8 hex>.lock`), so the machine's other
//! runner processes, which share the work directory (an upgrade's new build runs beside the old
//! one until the old one's legs end), leave it be too.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// A running leg's own tool cache (W06-R01). GitHub's self-hosted runner runs one job at a time,
/// so its tool cache is never written twice at once; a kiln runner runs several legs, and two
/// `setup-python`s installing the same version into one cache break each other. Each running
/// leg therefore holds one of the tool caches beside `base` (`base` itself, then `base-1`,
/// `base-2`, …): the first free one, kept for the legs after it.
pub struct ToolCache {
    base: PathBuf,
    n: usize,
}

static TOOL_LEASES: LazyLock<parking_lot::Mutex<HashMap<PathBuf, BTreeSet<usize>>>> = LazyLock::new(Default::default);

impl ToolCache {
    pub fn take(base: &Path) -> ToolCache {
        let mut leases = TOOL_LEASES.lock();
        let held = leases.entry(base.to_path_buf()).or_default();
        let n = (0..).find(|n| !held.contains(n)).unwrap_or(0);
        held.insert(n);
        ToolCache { base: base.to_path_buf(), n }
    }

    pub fn path(&self) -> PathBuf {
        if self.n == 0 {
            return self.base.clone();
        }
        let name = self.base.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        self.base.with_file_name(format!("{name}-{}", self.n))
    }
}

impl Drop for ToolCache {
    fn drop(&mut self) {
        if let Some(held) = TOOL_LEASES.lock().get_mut(&self.base) {
            held.remove(&self.n);
        }
    }
}

/// The file that keeps a failed leg's workspace, holding the Unix time it may go.
pub const KEEP_FILE: &str = ".kiln-keep-until";

/// Workspaces of the legs running in this process, each with its lock (`None`: it couldn't be
/// taken).
static IN_USE: LazyLock<parking_lot::Mutex<HashMap<PathBuf, Option<std::fs::File>>>> = LazyLock::new(Default::default);

fn in_use(p: &Path) -> bool {
    IN_USE.lock().contains_key(p) || held_elsewhere(p)
}

/// The lock a running leg holds beside its workspace.
fn lock_path(ws: &Path) -> PathBuf {
    let mut name = ws.file_name().map(std::ffi::OsStr::to_os_string).unwrap_or_default();
    name.push(".lock");
    ws.with_file_name(name)
}

/// Another process holds `ws`'s lock: a leg of another runner process of this machine runs in it.
fn held_elsewhere(ws: &Path) -> bool {
    let Ok(f) = std::fs::File::open(lock_path(ws)) else { return false };
    matches!(f.try_lock(), Err(std::fs::TryLockError::WouldBlock))
}

/// Take `ws`'s lock for a leg of this process.
fn lock(ws: &Path) -> Option<std::fs::File> {
    let path = lock_path(ws);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok()?;
    }
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path).ok()?;
    f.try_lock().ok()?;
    Some(f)
}

/// When a kept workspace may go, if it is kept.
pub fn kept_until(ws: &Path) -> Option<i64> {
    std::fs::read_to_string(ws.join(KEEP_FILE)).ok()?.trim().parse().ok()
}

/// Removes a workspace. Files a Docker action wrote as root are removed by a container, as root.
pub(super) fn remove(p: &Path) {
    if std::fs::remove_dir_all(p).is_ok() || !p.exists() {
        return;
    }
    let (Some(parent), Some(name), Some(bin)) = (p.parent(), p.file_name(), super::docker::find()) else {
        tracing::warn!(workspace = %p.display(), "a leg's workspace could not be removed");
        return;
    };
    let gone = std::process::Command::new(bin)
        .args(["run", "--rm", "--network", "none", "-v", &format!("{}:/w", parent.display()), "alpine:3", "rm", "-rf", &format!("/w/{}", name.to_string_lossy())])
        .output()
        .is_ok_and(|o| o.status.success());
    if !gone {
        tracing::warn!(workspace = %p.display(), "a leg's workspace could not be removed, even as root");
    }
}

/// What a sweep of `root` removes at `now`: the workspaces no running leg uses that aren't kept
/// past `now`.
fn sweep(root: &Path, now: i64) -> usize {
    let Ok(rd) = std::fs::read_dir(root) else { return 0 };
    let mut n = 0;
    for p in rd.filter_map(Result::ok).map(|e| e.path()) {
        if !p.is_dir() {
            // A lock whose workspace is gone, that no one holds.
            if p.extension().is_some_and(|x| x == "lock") && !p.with_extension("").exists() && !held_elsewhere(&p.with_extension("")) {
                let _ = std::fs::remove_file(&p);
            }
            // A leg's Docker socket (`docker_sock`) whose runner stopped short.
            if let Some(ws) = p.to_str().and_then(|s| s.strip_suffix(".docker.sock")).map(PathBuf::from)
                && !ws.exists()
                && !held_elsewhere(&ws)
            {
                let _ = std::fs::remove_file(&p);
            }
            continue;
        }
        if in_use(&p) || kept_until(&p).is_some_and(|t| t > now) {
            continue;
        }
        remove(&p);
        let _ = std::fs::remove_file(lock_path(&p));
        n += 1;
    }
    n
}

/// A leg starts using `ws` (under `<work>/w`): nothing removes it while it runs, and what earlier
/// legs left behind goes.
pub async fn claim(work: &Path, ws: &Path) {
    let held = lock(ws);
    if held.is_none() {
        tracing::warn!(workspace = %ws.display(), "a leg's workspace lock couldn't be taken: another runner process of this machine may sweep it");
    }
    IN_USE.lock().insert(ws.to_path_buf(), held);
    let root = work.join("w");
    let swept = crate::rt::blocking(move || sweep(&root, crate::util::now_unix())).await.unwrap_or(0);
    if swept > 0 {
        tracing::info!(swept, "removed workspaces earlier legs left behind");
    }
}

/// A leg is done with `ws` (or a kept one's keep ran out): it is removed, and the sweep leaves
/// it be no longer. A warm one goes back to its lane instead (docs/ci.md W24-R11).
pub async fn release(ws: &Path) {
    let held = IN_USE.lock().remove(ws);
    if super::warm::let_go(ws).await {
        let lock = lock_path(ws);
        let _ = crate::rt::blocking(move || {
            let _ = std::fs::remove_file(lock);
            drop(held);
        })
        .await;
        return;
    }
    let ws = ws.to_path_buf();
    let _ = crate::rt::blocking(move || {
        remove(&ws);
        let _ = std::fs::remove_file(lock_path(&ws));
        drop(held);
    })
    .await;
}

/// A failed leg's workspace is kept until `until` (Unix seconds): the sweep leaves it until then,
/// even after this runner restarts (W17-R02).
pub fn keep_until(ws: &Path, until: i64) {
    IN_USE.lock().remove(ws);
    if let Err(e) = std::fs::write(ws.join(KEEP_FILE), until.to_string()) {
        tracing::warn!(error = %e, "a failed leg's workspace could not be marked kept");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w06_r01_legs_running_at_once_get_tool_caches_of_their_own() {
        let base = PathBuf::from("/w/_tool/kiln-runner-ubuntu-24.04");
        let a = ToolCache::take(&base);
        let b = ToolCache::take(&base);
        assert_eq!((a.path(), b.path()), (base.clone(), PathBuf::from("/w/_tool/kiln-runner-ubuntu-24.04-1")));
        drop(a);
        let c = ToolCache::take(&base);
        assert_eq!(c.path(), base, "a free cache is taken again, with what it holds");
        let other = ToolCache::take(Path::new("/w/_tool"));
        assert_eq!(other.path(), PathBuf::from("/w/_tool"), "caches of another image are their own");
        drop((b, c, other));
    }

    #[test]
    fn w17_ac02_a_sweep_leaves_running_and_kept_workspaces() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("w");
        let (running, kept, expired, left) = (root.join("aaaa0001"), root.join("aaaa0002"), root.join("aaaa0003"), root.join("aaaa0004"));
        for p in [&running, &kept, &expired, &left] {
            std::fs::create_dir_all(p.join("repo"))?;
            std::fs::write(p.join("repo").join("file"), "x")?;
        }
        std::fs::write(kept.join(KEEP_FILE), "2000")?;
        std::fs::write(expired.join(KEEP_FILE), "500")?;
        // Legs' Docker sockets (files stand in for them): a running leg's, and one a runner that
        // stopped short left beside no workspace.
        let (live, stale) = (root.join("aaaa0001.docker.sock"), root.join("aaaa0009.docker.sock"));
        std::fs::write(&live, "")?;
        std::fs::write(&stale, "")?;
        IN_USE.lock().insert(running.clone(), None);
        assert_eq!(sweep(&root, 1000), 2);
        assert!(running.exists() && kept.exists() && !expired.exists() && !left.exists());
        assert!(live.exists() && !stale.exists());
        IN_USE.lock().remove(&running);
        assert_eq!(sweep(&root, 3000), 2, "once the leg ended and the keep ran out");
        assert!(!running.exists() && !kept.exists());
        Ok(())
    }

    #[test]
    fn a_sweep_leaves_the_workspace_of_a_leg_another_runner_process_runs() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("w");
        let (theirs, stale) = (root.join("bbbb0001"), root.join("bbbb0002"));
        for p in [&theirs, &stale] {
            std::fs::create_dir_all(p)?;
        }
        // The other process's lock (a lock taken through another open file conflicts all the same).
        let held = lock(&theirs).ok_or_else(|| anyhow::anyhow!("no lock"))?;
        // A lock left by a leg that ended without cleaning up.
        std::fs::write(lock_path(&stale), "")?;
        assert_eq!(sweep(&root, 1000), 1);
        assert!(theirs.exists() && !stale.exists() && !lock_path(&stale).exists());
        drop(held);
        // A process another test forks meanwhile holds a copy of the lock until it executes its
        // program: for a moment the lock may still look held (the next sweep takes it then).
        let mut swept = 0;
        for _ in 0..200 {
            swept += sweep(&root, 1000);
            if swept > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(swept, 1, "once that leg ended");
        assert!(!theirs.exists() && !lock_path(&theirs).exists());
        Ok(())
    }
}
