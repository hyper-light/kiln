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
