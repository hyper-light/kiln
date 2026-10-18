//! Legs' workspaces (docs/ci.md W06-R01, W17-R02). Each leg's `<work>/w/<8 hex>` is removed when
//! the leg ends, unless it failed under `keep-on-failure`, which keeps it that long: a
//! `.kiln-keep-until` file in it says until when. Each leg's start also sweeps what is left
//! behind: workspaces no running leg uses whose keep has run out, or that never had one (a runner
//! stopped mid-leg). The tool cache, actions and Node runtimes beside them stay (W06-R01).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

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

/// Workspaces of the legs running in this process.
static IN_USE: LazyLock<parking_lot::Mutex<HashSet<PathBuf>>> = LazyLock::new(Default::default);

fn in_use(p: &Path) -> bool {
    IN_USE.lock().contains(p)
}

/// When a kept workspace may go, if it is kept.
pub fn kept_until(ws: &Path) -> Option<i64> {
    std::fs::read_to_string(ws.join(KEEP_FILE)).ok()?.trim().parse().ok()
}

/// Removes a workspace. Files a Docker action wrote as root are removed by a container, as root.
fn remove(p: &Path) {
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
    for p in rd.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.is_dir()) {
        if in_use(&p) || kept_until(&p).is_some_and(|t| t > now) {
            continue;
        }
        remove(&p);
        n += 1;
    }
    n
}

/// A leg starts using `ws` (under `<work>/w`): nothing removes it while it runs, and what earlier
/// legs left behind goes.
pub async fn claim(work: &Path, ws: &Path) {
    IN_USE.lock().insert(ws.to_path_buf());
    let root = work.join("w");
    let swept = crate::rt::blocking(move || sweep(&root, crate::util::now_unix())).await.unwrap_or(0);
    if swept > 0 {
        tracing::info!(swept, "removed workspaces earlier legs left behind");
    }
}

/// A leg is done with `ws`: removed now, or kept `keep_minutes` (a failed leg's
/// `keep-on-failure`, W17-R02) and removed then, unless kept longer meanwhile.
pub async fn release(ws: &Path, keep_minutes: Option<u32>) {
    IN_USE.lock().remove(ws);
    let ws = ws.to_path_buf();
    match keep_minutes.filter(|m| *m > 0) {
        Some(m) => {
            let until = crate::util::now_unix() + i64::from(m) * 60;
            if let Err(e) = std::fs::write(ws.join(KEEP_FILE), until.to_string()) {
                tracing::warn!(error = %e, "a failed leg's workspace could not be kept");
            }
            crate::rt::spawn(async move {
                crate::rt::sleep(Duration::from_secs(u64::from(m) * 60)).await;
                let _ = crate::rt::blocking(move || {
                    if !in_use(&ws) && kept_until(&ws).is_none_or(|t| t <= crate::util::now_unix()) {
                        remove(&ws);
                    }
                })
                .await;
            });
        }
        None => {
            let _ = crate::rt::blocking(move || remove(&ws)).await;
        }
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
        IN_USE.lock().insert(running.clone());
        assert_eq!(sweep(&root, 1000), 2);
        assert!(running.exists() && kept.exists() && !expired.exists() && !left.exists());
        IN_USE.lock().remove(&running);
        assert_eq!(sweep(&root, 3000), 2, "once the leg ended and the keep ran out");
        assert!(!running.exists() && !kept.exists());
        Ok(())
    }
}
