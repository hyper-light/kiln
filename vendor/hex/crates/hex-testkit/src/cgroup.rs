//! The cgroup a Linux test process subdivides for its boxes: its own, delegated to it ([`delegated`]). It is
//! prepared by moving the process into a leaf of it ([`LEAF`]), so the cgroup holds no process of its own and the
//! kernel lets it hand its controllers down, then by the caller's probe that every controller its boxes use is
//! handed down (hex-cli's tests open a `BoxRoot` there).
//!
//! Once per process (ROOT's finding of 2026-11-13). A test binary's tests run on threads of one process, and a cgroup
//! holds a process whole, so a preparation per test raced: one that read the process's cgroup after another's move
//! took the leaf for its own. And a child a test started before the preparation (oci_box's `id -u`) stayed in the
//! cgroup when the process moved, where the kernel refuses to enable a controller for a cgroup's children while it
//! holds a process (EBUSY), so the test skipped. The preparation now moves every process of this one's still there.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The leaf the test process is moved into, and with it every process it has started; those it starts later are
/// born there.
pub const LEAF: &str = "hex-agent";

/// The process's one preparation, every caller's.
static PREPARED: OnceLock<Result<PathBuf, String>> = OnceLock::new();

/// This process's cgroup directory under /sys/fs/cgroup, as /proc/self/cgroup names it now.
pub fn own() -> Result<PathBuf, String> {
    let listed = fs::read_to_string("/proc/self/cgroup")
        .map_err(|e| format!("reading /proc/self/cgroup: {e}"))?;
    let rel = listed
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .ok_or_else(|| "no cgroup v2 line (0::) in /proc/self/cgroup".to_owned())?;
    Ok(Path::new("/sys/fs/cgroup").join(rel.trim_start_matches('/')))
}

/// This process's own cgroup, delegated to it, prepared once per process. The first call reads the cgroup the process
/// is in, moves the process into its [`LEAF`] and with it every process of its own still there (a child another test
/// started before the move, and that child's own), then runs `probe` on it. Every call, at once or later, gets that
/// one result, its own `probe` unrun. A process not this one's (a shell that started the test binary in the same
/// cgroup) is left, for the probe to refuse by name.
pub fn delegated(probe: impl FnOnce(&Path) -> Result<(), String>) -> Result<PathBuf, String> {
    PREPARED.get_or_init(|| prepare(probe)).clone()
}

fn prepare(probe: impl FnOnce(&Path) -> Result<(), String>) -> Result<PathBuf, String> {
    let own = own()?;
    let leaf = own.join(LEAF);
    fs::create_dir_all(&leaf).map_err(|e| format!("making a leaf for the agent: {e}"))?;
    let me = std::process::id();
    move_into(&leaf, me).map_err(|e| format!("moving self into a leaf: {e}"))?;
    // A process of this one's still in `own` was started before the move; one it starts until it is moved is born
    // there too, so `own` is read again until it lists none not yet moved. Each is moved once: one the kernel keeps
    // listing (a group leader that ended while its threads live) is left, for the probe to refuse by name.
    let mut moved = BTreeSet::new();
    loop {
        let left: Vec<u32> = descendants_in(&own, me)?
            .into_iter()
            .filter(|pid| moved.insert(*pid))
            .collect();
        if left.is_empty() {
            break;
        }
        for pid in left {
            match move_into(&leaf, pid) {
                Ok(()) => {}
                // It ended since `own` listed it.
                Err(e) if e.raw_os_error() == Some(libc::ESRCH) => {}
                Err(e) => {
                    return Err(format!(
                        "moving process {pid}, one of this one's, into a leaf: {e}"
                    ));
                }
            }
        }
    }
    probe(&own)?;
    Ok(own)
}

/// Moves process `pid`, every thread of it, into `cgroup`.
fn move_into(cgroup: &Path, pid: u32) -> io::Result<()> {
    fs::write(cgroup.join("cgroup.procs"), pid.to_string())
}

/// The processes in `cgroup` itself (not its children) that descend from process `me`.
fn descendants_in(cgroup: &Path, me: u32) -> Result<Vec<u32>, String> {
    let listed = fs::read_to_string(cgroup.join("cgroup.procs"))
        .map_err(|e| format!("listing {}'s processes: {e}", cgroup.display()))?;
    Ok(listed
        .lines()
        .filter_map(|l| l.trim().parse::<u32>().ok())
        .filter(|&pid| descends(pid, me))
        .collect())
}

/// Whether process `pid` descends from process `me`: `me` is among its parents, read up to init, to a process that has
/// ended (one whose parent can't be read is no longer anyone's to move), or to one read before (a pid reused while
/// the parents were read).
fn descends(pid: u32, me: u32) -> bool {
    let mut seen = BTreeSet::from([pid]);
    let mut at = pid;
    while let Some(parent) = parent_of(at) {
        if parent == me {
            return true;
        }
        if parent <= 1 || !seen.insert(parent) {
            return false;
        }
        at = parent;
    }
    false
}

/// Process `pid`'s parent, from its /proc status's `PPid:` line; `None` once it has ended.
fn parent_of(pid: u32) -> Option<u32> {
    fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("PPid:"))?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests;
