//! Windows.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::FileExt as _;
use std::path::{Path, PathBuf};

// ------------------------------------------------------------------------- identity and paths

/// kiln never installs system-wide on Windows: everything is per-user.
pub fn is_root() -> bool {
    false
}

/// Whether this process runs with an administrator's full rights (elevated, or UAC is off): it
/// may register a Scheduled Task that starts kiln at boot.
pub fn elevated() -> bool {
    super::win32::elevated()
}

/// Run `program` with `parameters` (one command line) as an administrator, its window hidden,
/// and wait up to `timeout` for its exit code. Windows asks the person at the desktop first
/// (UAC); an error of kind `PermissionDenied` means they said no.
pub fn run_elevated(program: &Path, parameters: &str, timeout: std::time::Duration) -> io::Result<u32> {
    super::win32::run_elevated(program, parameters, timeout).map_err(|e| match e.raw_os_error() {
        Some(super::win32::ERROR_CANCELLED) => io::Error::new(io::ErrorKind::PermissionDenied, "the administrator prompt was declined"),
        _ => e,
    })
}

/// Put `dir` on the user's PATH (`HKCU\Environment`), for what starts from now on: new
/// terminals, and Explorer, which is told it changed. `Ok(None)` when it was on it already.
pub fn add_to_user_path(dir: &Path) -> io::Result<Option<super::PathAdded>> {
    let (list, kind) = super::win32::user_path()?.unwrap_or((String::new(), super::win32::REG_EXPAND_SZ));
    let Some(list) = super::path_list_with(&list, &dir.display().to_string(), |name| std::env::var(name).ok()) else {
        return Ok(None);
    };
    super::win32::set_user_path(&list, kind)?;
    super::win32::announce_environment();
    Ok(Some(super::PathAdded { file: None, now: None, created: false }))
}

/// Take `dir` back off the user's PATH, as [`add_to_user_path`] put it there.
pub fn remove_from_user_path(dir: &Path, _added: &super::PathAdded) -> io::Result<()> {
    let Some((list, kind)) = super::win32::user_path()? else { return Ok(()) };
    let Some(list) = super::path_list_without(&list, &dir.display().to_string(), |name| std::env::var(name).ok()) else {
        return Ok(());
    };
    super::win32::set_user_path(&list, kind)?;
    super::win32::announce_environment();
    Ok(())
}

/// Windows identifies users by SID, not a numeric id; nothing on Windows asks for one.
pub fn uid() -> u32 {
    0
}

/// `DOMAIN\user` of the current user (a Scheduled Task's principal).
pub fn user_principal() -> String {
    let user = std::env::var("USERNAME").unwrap_or_default();
    match std::env::var("USERDOMAIN") {
        Ok(domain) if !domain.is_empty() => format!("{domain}\\{user}"),
        _ => user,
    }
}

pub fn hostname() -> String {
    sysinfo::System::host_name()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "localhost".to_owned())
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// System-wide data directory and config file. Unused: [`is_root`] is always false.
pub fn system_dirs() -> (PathBuf, PathBuf) {
    let base = std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    (base.join("kiln"), base.join("kiln").join("kiln.toml"))
}

/// Per-user, machine-local data (`%LOCALAPPDATA%`).
pub fn data_home() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join("AppData").join("Local"))
}

/// Per-user config (`%APPDATA%`).
pub fn config_home() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join("AppData").join("Roaming"))
}

/// Per-user state (`%LOCALAPPDATA%`).
pub fn state_home() -> PathBuf {
    data_home()
}

// ---------------------------------------------------------------------------------------- files

/// Windows has no permission bits; files under the user's profile are private to the user.
pub fn with_mode(opts: &mut OpenOptions, _mode: u32) -> &mut OpenOptions {
    opts
}

/// Nothing to do: whether a file runs is decided by its extension.
pub fn make_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Open `path` to read it, sharing it for reading only: until the file is closed nobody can
/// write, rename or delete it.
pub fn open_unchangeable(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt as _;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    OpenOptions::new().read(true).share_mode(FILE_SHARE_READ).open(path)
}

/// Read at `offset`. (`seek_read` also moves the handle's cursor; kiln never mixes it with
/// cursor-based I/O on the same handle.)
pub fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    file.seek_read(buf, offset)
}

/// Write all of `buf` at `offset`.
pub fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        match file.seek_write(buf, offset) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => {
                buf = buf.get(n..).unwrap_or_default();
                offset = offset.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

// ------------------------------------------------------------------- in-place re-execution

/// Windows has no exec: upgrades restart the process (and its terminals).
pub const IN_PLACE_UPGRADE: bool = false;

/// Terminals are never handed to a successor on Windows (see the Unix layer).
pub const UPGRADE_BY_SUCCESSOR: bool = false;

pub fn keep_across_exec(_fd: &()) -> io::Result<()> {
    Err(io::Error::other("in-place upgrade is not supported on Windows"))
}

pub fn reclaim_fd(_raw: i32) -> Option<()> {
    None
}

pub fn reexec(_exe: &Path, _args: &[std::ffi::OsString], _env: &[(&str, String)]) -> io::Error {
    io::Error::other("in-place upgrade is not supported on Windows")
}

/// Windows keeps a finished child's exit in its handle: there are no zombies to reap.
pub fn start_reaper() {}

pub fn pid_alive(_pid: u32) -> bool {
    false
}

/// Not needed on Windows (no launchd); 0.
pub fn parent_pid() -> u32 {
    0
}

/// Process trees are job objects on Windows; nothing to stop one by one.
pub fn stop_processes(_pids: &[u32], _keep: Option<u32>, _grace: std::time::Duration) {}

pub fn try_reap(_pid: u32) -> io::Result<Option<(Option<i32>, String)>> {
    Err(io::Error::other("unsupported on Windows"))
}

/// How many PTY masters process `pid` holds open: not something Windows can tell from outside.
pub fn pty_masters(_pid: u32) -> Option<usize> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The user's PATH for real (by hand): a folder goes on it once, the value keeps its registry
    /// type, and the PATH is put back as it was.
    #[test]
    #[ignore = "changes this user's PATH, then puts it back"]
    fn a_folder_goes_on_the_users_path_once() -> anyhow::Result<()> {
        let Some((before, kind)) = super::super::win32::user_path()? else {
            anyhow::bail!("this user has no PATH of their own to try it on");
        };
        let dir = std::env::temp_dir().join(format!("kiln path test {}", std::process::id()));
        let result = (|| -> anyhow::Result<()> {
            anyhow::ensure!(add_to_user_path(&dir)?.is_some(), "not added");
            let (after, after_kind) = super::super::win32::user_path()?.ok_or_else(|| anyhow::anyhow!("PATH gone"))?;
            anyhow::ensure!(after_kind == kind, "registry type {kind} became {after_kind}");
            anyhow::ensure!(after == format!("{};{}", before.trim_end_matches(';'), dir.display()), "PATH is now {after}");
            anyhow::ensure!(add_to_user_path(&dir)?.is_none(), "added twice");
            Ok(())
        })();
        super::super::win32::set_user_path(&before, kind)?;
        super::super::win32::announce_environment();
        result
    }
}
