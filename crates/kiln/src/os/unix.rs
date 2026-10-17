//! Unix (Linux and macOS).

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

// ------------------------------------------------------------------------- identity and paths

/// Running as root (system-wide install, `/etc/kiln`, `/var/lib/kiln`).
pub fn is_root() -> bool {
    nix::unistd::geteuid().is_root()
}

/// The real user id (launchd's per-user domains are `gui/<uid>` and `user/<uid>`).
pub fn uid() -> u32 {
    nix::unistd::getuid().as_raw()
}

/// The current user's login name.
pub fn user_principal() -> String {
    std::env::var("USER").unwrap_or_default()
}

pub fn hostname() -> String {
    nix::unistd::gethostname()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "localhost".to_owned())
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

/// System-wide data directory and config file (used when running as root).
pub fn system_dirs() -> (PathBuf, PathBuf) {
    (PathBuf::from("/var/lib/kiln"), PathBuf::from("/etc/kiln/kiln.toml"))
}

/// Per-user data (`$XDG_DATA_HOME`, else `~/.local/share`).
pub fn data_home() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Per-user config (`$XDG_CONFIG_HOME`, else `~/.config`).
pub fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Per-user state (`$XDG_STATE_HOME` when absolute, else `~/.local/state`).
pub fn state_home() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from(".local/state"))
}

// ---------------------------------------------------------------------------------------- files

/// Create files with Unix permission bits `mode` (e.g. `0o600` for secrets).
pub fn with_mode(opts: &mut OpenOptions, mode: u32) -> &mut OpenOptions {
    opts.mode(mode)
}

/// Mark a file executable (`0755`).
pub fn make_executable(path: &Path) -> io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

/// Read at `offset` without moving a shared cursor.
pub fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    file.read_at(buf, offset)
}

/// Write all of `buf` at `offset` without moving a shared cursor.
pub fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    file.write_all_at(buf, offset)
}

// ------------------------------------------------------------------- in-place re-execution

/// This platform can replace a running kiln with a new binary in place (same pid), handing open
/// PTY masters to the new image so shells survive the upgrade.
pub const IN_PLACE_UPGRADE: bool = true;

/// Keep `fd` open across the next exec (clear close-on-exec).
pub fn keep_across_exec(fd: &std::os::fd::OwnedFd) -> io::Result<()> {
    rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::empty()).map_err(io::Error::from)
}

/// Take ownership of a descriptor this process carried across its own re-exec (its number comes
/// from the handoff file the previous image wrote). `None` when that number is not open.
#[allow(unsafe_code)]
pub fn reclaim_fd(raw: i32) -> Option<std::os::fd::OwnedFd> {
    use std::os::fd::{BorrowedFd, FromRawFd as _};
    if raw < 3 {
        return None;
    }
    // SAFETY: the number was written by this same process's previous image just before it
    // re-executed itself, for a descriptor it had cleared close-on-exec on; the handoff file is
    // read once at start-up and deleted, and nothing else in the new image has claimed that
    // number yet. The borrow only lives for the validity check.
    let open = rustix::io::fcntl_getfd(unsafe { BorrowedFd::borrow_raw(raw) }).is_ok();
    // SAFETY: as above; the descriptor is open and owned by nobody else in this image.
    open.then(|| unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) })
}

/// Replace this process image with `exe` (same pid, same children). Returns only on failure.
pub fn reexec(exe: &Path, args: &[std::ffi::OsString], env: &[(&str, String)]) -> io::Error {
    use std::os::unix::process::CommandExt as _;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.exec()
}

/// Stop these processes and the process groups they lead or belong to (never `keep`'s, nor this
/// process's own): SIGTERM now, SIGKILL to whatever is left after `grace`. Blocking.
pub fn stop_processes(pids: &[u32], keep: Option<u32>, grace: std::time::Duration) {
    use nix::sys::signal::{Signal, kill, killpg};
    use nix::unistd::{Pid, getpgid};
    let ours = getpgid(None).ok();
    let keep = keep.and_then(|k| i32::try_from(k).ok()).map(Pid::from_raw);
    let mut groups = std::collections::HashSet::new();
    let pids: Vec<Pid> = pids.iter().filter_map(|p| i32::try_from(*p).ok()).filter(|p| *p > 1).map(Pid::from_raw).collect();
    for p in &pids {
        if let Ok(g) = getpgid(Some(*p))
            && Some(g) != ours
            && Some(g) != keep
        {
            groups.insert(g);
        }
        let _ = kill(*p, Signal::SIGTERM);
    }
    for g in &groups {
        let _ = killpg(*g, Signal::SIGTERM);
    }
    let end = std::time::Instant::now() + grace;
    while std::time::Instant::now() < end && pids.iter().any(|p| kill(*p, None).is_ok()) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    for p in &pids {
        let _ = kill(*p, Signal::SIGKILL);
    }
    for g in &groups {
        let _ = killpg(*g, Signal::SIGKILL);
    }
}

/// Whether `pid` is still running (and signalable by us).
pub fn pid_alive(pid: u32) -> bool {
    i32::try_from(pid).is_ok_and(|p| nix::sys::signal::kill(nix::unistd::Pid::from_raw(p), None).is_ok())
}

/// Reap `pid` if it has exited: `Ok(Some(description))` once it has, `Ok(None)` while it runs.
pub fn try_reap(pid: u32) -> io::Result<Option<(Option<i32>, String)>> {
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
    let p = nix::unistd::Pid::from_raw(i32::try_from(pid).map_err(|_| io::Error::other("bad pid"))?);
    match waitpid(p, Some(WaitPidFlag::WNOHANG)) {
        Ok(WaitStatus::StillAlive) => Ok(None),
        Ok(WaitStatus::Exited(_, code)) => Ok(Some((Some(code), format!("exit status: {code}")))),
        Ok(WaitStatus::Signaled(_, sig, _)) => Ok(Some((None, format!("signal: {sig}")))),
        Ok(other) => Ok(Some((None, format!("{other:?}")))),
        Err(e) => Err(io::Error::from(e)),
    }
}
