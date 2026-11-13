//! The host folders a node's Shards guest sees live (docs/design/shards-guest.md §4a): Shards' virtio-fs
//! shares, fixed at the guest's boot and mounted by its init at `/run/hex/shares/<i>`. A bind mount's
//! source is rewritten to its path under the share that holds it; a source under none is refused by
//! name, naming the shared roots. The guest learns the shares from its kernel's command line
//! ([`CMDLINE`]).
//!
//! The host's `/` is never shared by default: that would hand any process in the guest, a container's
//! escape included, everything the node's user can write. The default is Docker Desktop's file-sharing
//! list (`docs/research/runtime/shards-guest.md` §7): on macOS `/Users`, `/Volumes`, `/private`, `/tmp` and
//! `/var/folders`; on Windows the users folder and the temp folder; read-write, served as the node's own
//! user. Any other root is the operator's, in `HEX_GUEST_SHARES`.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use hex_api::Refusal;

/// The guest's folder holding the shares, `<i>` under it for the `i`th.
pub const SHARES: &str = "/run/hex/shares";
/// The most roots a node shares: one virtio-fs device each, beside the vsock device, among Shards'
/// virtio-mmio slots for the guest's architecture, the host's (shards a0cced87
/// `crates/vmm/src/arch/x86_64/mod.rs:47` `VIRTIO_MMIO_MAX` 18, the free IOAPIC pins;
/// `crates/vmm/src/arch/aarch64/mod.rs:33`, 32), so one fewer. It is the guest init's bound on its
/// `hex.shares=` (HEX-GUEST-11), so a longer list is refused here by name, before a guest is booted
/// that would refuse it.
#[cfg(target_arch = "x86_64")]
pub const MAX_SHARES: usize = 18 - 1;
#[cfg(not(target_arch = "x86_64"))]
pub const MAX_SHARES: usize = 32 - 1;
/// The kernel command-line parameter naming each share's mode, in tag order, for the guest's init.
pub const CMDLINE: &str = "hex.shares";
/// The environment variable naming the node's shared roots, in place of the default.
pub const ENV: &str = "HEX_GUEST_SHARES";

/// A host folder shared into the guest, and where the guest sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Share {
    pub dir: PathBuf,
    pub read_only: bool,
    /// Its path in the guest: `/run/hex/shares/<i>` for the `i`th, where the guest's init mounts it.
    pub at: String,
}

/// `dirs` as shares, each where the guest's init mounts it by its place in the list.
fn numbered(dirs: impl IntoIterator<Item = (PathBuf, bool)>) -> Vec<Share> {
    dirs.into_iter()
        .enumerate()
        .map(|(i, (dir, read_only))| Share {
            dir,
            read_only,
            at: format!("{SHARES}/{i}"),
        })
        .collect()
}

/// The shared roots: `HEX_GUEST_SHARES` when set, else this OS's default.
pub fn from_env() -> Result<Vec<Share>, Refusal> {
    from_var(std::env::var_os(ENV))
}

/// The shared roots `HEX_GUEST_SHARES`' value names, or the default where it has none.
pub fn from_var(list: Option<std::ffi::OsString>) -> Result<Vec<Share>, Refusal> {
    match list {
        Some(list) => parse(&list),
        None => defaults(),
    }
}

/// A list of roots as `HEX_GUEST_SHARES` gives them: paths split by the OS's path-list separator
/// (`:` on Unix, `;` on Windows, as `PATH`'s), each read-only when it ends `:ro` or is followed by an
/// element `ro` (so `/a:ro:/b` reads on Unix as it looks). Each must be absolute; the list must not be
/// empty or past [`MAX_SHARES`].
pub fn parse(list: &OsStr) -> Result<Vec<Share>, Refusal> {
    let invalid = |why: String| Refusal::Invalid { what: ENV, why };
    let mut shares: Vec<(PathBuf, bool)> = Vec::new();
    for element in std::env::split_paths(list) {
        let text = element
            .to_str()
            .ok_or_else(|| invalid(format!("{} isn't UTF-8", element.display())))?;
        if text.is_empty() {
            continue;
        }
        if text == "ro" {
            let last = shares
                .last_mut()
                .ok_or_else(|| invalid("`ro` follows no path".to_owned()))?;
            last.1 = true;
            continue;
        }
        let (dir, read_only) = match text.strip_suffix(":ro") {
            Some(dir) => (dir, true),
            None => (text, false),
        };
        let dir = PathBuf::from(dir);
        if !dir.is_absolute() {
            return Err(invalid(format!("{} isn't an absolute path", dir.display())));
        }
        if shares.len() >= MAX_SHARES {
            return Err(invalid(format!("it names more than {MAX_SHARES} roots")));
        }
        shares.push((dir, read_only));
    }
    if shares.is_empty() {
        return Err(invalid("it names no root".to_owned()));
    }
    Ok(numbered(shares))
}

/// This OS's default roots, Docker Desktop's file-sharing list, read-write.
pub fn defaults() -> Result<Vec<Share>, Refusal> {
    Ok(numbered(
        default_dirs()?.into_iter().map(|dir| (dir, false)),
    ))
}

#[cfg(target_os = "macos")]
fn default_dirs() -> Result<Vec<PathBuf>, Refusal> {
    Ok(["/Users", "/Volumes", "/private", "/tmp", "/var/folders"]
        .into_iter()
        .map(PathBuf::from)
        .collect())
}

/// The users folder, from its known folder (`FOLDERID_UserProfiles`, never a literal `C:\Users`), and
/// the temp folder (`GetTempPath2W`).
#[cfg(target_os = "windows")]
fn default_dirs() -> Result<Vec<PathBuf>, Refusal> {
    let users = crate::windows::sys::user_profiles()
        .map_err(|e| Refusal::os("finding the users folder (FOLDERID_UserProfiles)", e))?;
    let temp = crate::windows::sys::temp_folder()
        .map_err(|e| Refusal::os("finding the temp folder (GetTempPath2W)", e))?;
    Ok(vec![users, temp])
}

/// A Linux node runs the `oci` class natively and boots no guest: it shares nothing unless told.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn default_dirs() -> Result<Vec<PathBuf>, Refusal> {
    Ok(Vec::new())
}

/// `source`'s path in the guest, under the deepest share that holds it, or `None` when none does. A
/// source with a `..` or `.` past its share's root is under none: it is never resolved on the host.
pub fn in_guest(shares: &[Share], source: &Path) -> Option<String> {
    shares
        .iter()
        .filter_map(|share| {
            let rest = source.strip_prefix(&share.dir).ok()?;
            let mut path = share.at.trim_end_matches('/').to_owned();
            for part in rest.components() {
                let Component::Normal(name) = part else {
                    return None;
                };
                path.push('/');
                path.push_str(name.to_str()?);
            }
            if path.is_empty() {
                path.push('/');
            }
            Some((share.dir.components().count(), path))
        })
        .max_by_key(|(depth, _)| *depth)
        .map(|(_, path)| path)
}

/// The refusal of a bind mount whose source is under no share, naming it and the shared roots.
pub fn unshared(shares: &[Share], destination: &str, source: &Path) -> Refusal {
    let roots: Vec<String> = shares.iter().map(|s| s.dir.display().to_string()).collect();
    Refusal::Platform {
        what: "a bind mount of a host path under no shared root",
        platform: std::env::consts::OS,
        why: format!(
            "{destination}'s source {} is under none of the guest's shared roots [{}]; {ENV} sets them",
            source.display(),
            roots.join(", ")
        ),
    }
}
