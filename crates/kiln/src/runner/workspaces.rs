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

/// Workspaces admitted in this process, each holding its cross-process reservation (U03/U07).
static IN_USE: LazyLock<parking_lot::Mutex<HashMap<PathBuf, std::fs::File>>> = LazyLock::new(Default::default);

/// The lock a running leg holds beside its workspace.
fn lock_path(ws: &Path) -> PathBuf {
    let mut name = ws.file_name().map(std::ffi::OsStr::to_os_string).unwrap_or_default();
    name.push(".lock");
    ws.with_file_name(name)
}

/// U03/U07: never unlink this sidecar, even after deleting the workspace. Every contender
/// must open the same inode, and retain its reservation through all workspace mutations.
fn lock(ws: &Path) -> anyhow::Result<std::fs::File> {
    use anyhow::Context as _;
    let path = lock_path(ws);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create workspace lock directory {}", dir.display()))?;
    }
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path)
        .with_context(|| format!("open workspace lock {}", path.display()))?;
    f.try_lock().with_context(|| format!("reserve workspace {}", ws.display()))?;
    Ok(f)
}

/// Failed admission must neither replace a live reservation nor authorize workspace use.
fn admit(ws: &Path) -> anyhow::Result<()> {
    let mut in_use = IN_USE.lock();
    anyhow::ensure!(!in_use.contains_key(ws), "workspace {} is already admitted", ws.display());
    let held = lock(ws)?;
    in_use.insert(ws.to_path_buf(), held);
    Ok(())
}

/// When a kept workspace may go, if it is kept.
pub fn kept_until(ws: &Path) -> Option<i64> {
    std::fs::read_to_string(ws.join(KEEP_FILE)).ok()?.trim().parse().ok()
}

/// Removes files owned by the runner. Root-owned Docker files use the existing managed
/// execution's preplanned utility before its resource lease closes, never an unowned helper.
pub(super) fn remove(p: &Path) {
    if std::fs::remove_dir_all(p).is_ok() || !p.exists() {
        return;
    }
    tracing::warn!(workspace = %p.display(), "a leg's workspace needs its managed executor cleanup; its files remain available");
}

/// What a sweep of `root` removes at `now`: the workspaces no running leg uses that aren't kept
/// past `now`.
fn sweep(root: &Path, now: i64) -> usize {
    let Ok(rd) = std::fs::read_dir(root) else { return 0 };
    let mut n = 0;
    for p in rd.filter_map(Result::ok).map(|e| e.path()) {
        if !p.is_dir() {
            // A leg's Docker socket (`docker_sock`) whose runner stopped short.
            if let Some(ws) = p.to_str().and_then(|s| s.strip_suffix(".docker.sock")).map(PathBuf::from) {
                let Ok(_held) = lock(&ws) else { continue };
                if !ws.exists() {
                    let _ = std::fs::remove_file(&p);
                }
            }
            continue;
        }
        let Ok(_held) = lock(&p) else { continue };
        if kept_until(&p).is_some_and(|t| t > now) {
            continue;
        }
        remove(&p);
        n += 1;
    }
    n
}

/// A leg starts using `ws` (under `<work>/w`): nothing removes it while it runs, and what earlier
/// legs left behind goes.
pub async fn claim(work: &Path, ws: &Path) -> anyhow::Result<()> {
    admit(ws)?;
    let root = work.join("w");
    let swept = crate::rt::blocking(move || sweep(&root, crate::util::now_unix())).await.unwrap_or(0);
    if swept > 0 {
        tracing::info!(swept, "removed workspaces earlier legs left behind");
    }
    Ok(())
}

/// A leg is done with `ws` (or a kept one's keep ran out): it is removed, and the sweep leaves
/// it be no longer. A warm one goes back to its lane instead (docs/ci.md W24-R11).
pub async fn release(ws: &Path) {
    release_with_cleanup(ws, None).await;
}

pub(super) async fn release_with_cleanup(ws: &Path, cleanup: Option<super::docker::WorkspaceCleanup>) {
    // A kept workspace no longer has an in-process lease: reserve it again before cleanup.
    let held = IN_USE.lock().remove(ws);
    let Ok(held) = held.map_or_else(|| lock(ws), Ok) else {
        tracing::warn!(workspace = %ws.display(), "refusing cleanup without a workspace reservation");
        return;
    };
    if super::warm::let_go(ws).await {
        drop(held);
        return;
    }
    if let Some(cleanup) = cleanup
        && let Err(e) = cleanup.run().await
    {
        // U03/U07: an error is not physical completion proof. Keep this same reservation
        // while the existing daemon owner retains the unresolved cleanup intent.
        IN_USE.lock().insert(ws.to_path_buf(), held);
        tracing::error!(error = %e, workspace = %ws.display(), "the reserved workspace utility could not prove completion; retaining the workspace reservation and daemon ownership fence");
        return;
    }
    let ws = ws.to_path_buf();
    let _ = crate::rt::blocking(move || {
        remove(&ws);
        drop(held);
    })
    .await;
}

/// A failed leg's workspace is kept until `until` (Unix seconds): the sweep leaves it until then,
/// even after this runner restarts (W17-R02).
pub fn keep_until(ws: &Path, until: i64) {
    let held = IN_USE.lock().remove(ws);
    let Ok(held) = held.map_or_else(|| lock(ws), Ok) else {
        tracing::warn!(workspace = %ws.display(), "refusing keep without a workspace reservation");
        return;
    };
    if let Err(e) = std::fs::write(ws.join(KEEP_FILE), until.to_string()) {
        tracing::warn!(error = %e, "a failed leg's workspace could not be marked kept");
    }
    drop(held);
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
        admit(&running)?;
        assert_eq!(sweep(&root, 1000), 2);
        assert!(running.exists() && kept.exists() && !expired.exists() && !left.exists());
        assert!(live.exists() && !stale.exists());
        IN_USE.lock().remove(&running);
        assert_eq!(sweep(&root, 3000), 2, "once the leg ended and the keep ran out");
        assert!(!running.exists() && !kept.exists());
        Ok(())
    }

    #[test]
    fn u03_u07_claim_refuses_contention_and_failed_lock_admission() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let ws = dir.path().join("w").join("cccc0001");
        let held = lock(&ws)?;
        assert!(admit(&ws).is_err());
        assert!(!IN_USE.lock().contains_key(&ws));
        assert!(!ws.exists(), "failed admission never starts using a workspace");
        drop(held);
        admit(&ws)?;
        assert!(admit(&ws).is_err(), "duplicate admission must not replace the original owner");
        assert!(lock(&ws).is_err());
        IN_USE.lock().remove(&ws);
        let bad = dir.path().join("not-a-directory");
        std::fs::write(&bad, "x")?;
        let ws = bad.join("cccc0002");
        assert!(admit(&ws).is_err());
        assert!(!IN_USE.lock().contains_key(&ws));
        Ok(())
    }

    #[test]
    fn u03_u07_release_and_keep_preserve_the_lock_and_refuse_contention() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let ws = dir.path().join("w").join("dddd0001");
            std::fs::create_dir_all(&ws)?;
            claim(dir.path(), &ws).await?;
            let waiting = std::fs::OpenOptions::new().write(true).open(lock_path(&ws))?;
            keep_until(&ws, 2000);
            assert_eq!(kept_until(&ws), Some(2000));
            waiting.try_lock()?;
            release(&ws).await;
            keep_until(&ws, 3000);
            assert!(ws.exists(), "cleanup cannot delete a workspace reserved through another descriptor");
            assert_eq!(kept_until(&ws), Some(2000), "keep cannot mutate another owner's workspace");
            drop(waiting);
            // Kept workspace release reacquires a reservation before deleting it.
            release(&ws).await;
            assert!(!ws.exists() && lock_path(&ws).exists());
            let waiting = std::fs::OpenOptions::new().write(true).open(lock_path(&ws))?;
            std::fs::create_dir_all(&ws)?;
            claim(dir.path(), &ws).await?;
            release(&ws).await;
            waiting.try_lock()?;
            assert!(lock(&ws).is_err(), "release preserves the inode already opened by a contender");
            Ok(())
        })
    }

    #[test]
    fn u03_u07_a_sweep_leaves_the_workspace_of_a_leg_another_runner_process_runs() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("w");
        let (theirs, stale) = (root.join("bbbb0001"), root.join("bbbb0002"));
        for p in [&theirs, &stale] {
            std::fs::create_dir_all(p)?;
        }
        // The other process's lock (a lock taken through another open file conflicts all the same).
        let held = lock(&theirs)?;
        // A lock left by a leg that ended without cleaning up.
        std::fs::write(lock_path(&stale), "")?;
        // A contender may already have opened the inode before the sweep acquires it.
        let waiting = std::fs::OpenOptions::new().write(true).open(lock_path(&stale))?;
        assert_eq!(sweep(&root, 1000), 1);
        assert!(theirs.exists() && !stale.exists() && lock_path(&stale).exists());
        waiting.try_lock()?;
        assert!(lock(&stale).is_err(), "a pre-opened descriptor still fences new contenders after deletion");
        drop(waiting);
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
        assert!(!theirs.exists() && lock_path(&theirs).exists());
        Ok(())
    }
}
