//! Windows.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::FileExt as _;
use std::path::{Path, PathBuf};

// ------------------------------------------------------------------------- identity and paths

/// kiln never installs system-wide on Windows (no elevation): everything is per-user.
pub fn is_root() -> bool {
    false
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
