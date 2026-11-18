//! Where a workspace's volume is mounted on its machine (docs/agentic.md W1, W3; kiln goal
//! 01a1ec34599974448aed2672bafaf9a2, Windows): a folder of its own under the machine's workspaces folder on Linux
//! (FUSE) and macOS (NFS on loopback), and a drive letter of its own on Windows (WinFsp).
//!
//! Slates mounts a WinFsp volume only at a drive letter: a directory mount point would be an NTFS reparse point, a
//! write to the disk it refuses (Slates' SLATES_DESIGN.md D-3 and §4.6, "directory mounts refused"). So a Windows
//! machine holds as many workspaces at once as it has drive letters free, and each is taken from the top of the
//! alphabet down: Windows gives a new disk or a removable drive the lowest letter free, so the highest are the least
//! likely to be wanted next. Two agents starting at once can find the same letter free; Slates refuses the second
//! mount there, and its maker takes the next letter free ([`points`]).
//!
//! What is mounted at a point is judged by the system's own word, never by a path's spelling: a mount's device differs
//! from its parent's (Unix), or the drive is there and Windows names its file system Slates' (`slates`, the name its
//! WinFsp volume gives).

use std::path::{Path, PathBuf};

/// How a volume is mounted here, when it can be: `fuse` (Linux, through `fusermount3`, no privilege), `nfs` (macOS,
/// on loopback) or `winfsp` (Windows, through WinFsp's driver, installed once).
pub fn kind() -> Option<&'static str> {
    if cfg!(target_os = "linux") {
        crate::os::which("fusermount3").map(|_| "fuse")
    } else if cfg!(target_os = "macos") {
        Some("nfs")
    } else if cfg!(windows) {
        winfsp_installed().then_some("winfsp")
    } else {
        None
    }
}

/// Why no volume can be mounted here ([`kind`] says none), as a person can act on it.
pub fn why_not() -> &'static str {
    if cfg!(target_os = "linux") {
        "fusermount3 isn't installed (the fuse3 package)"
    } else if cfg!(windows) {
        "WinFsp isn't installed (winfsp.dev): Slates mounts a volume on Windows through it"
    } else {
        "Slates can't mount a volume on this system yet"
    }
}

/// The highest drive letter `held` leaves free, `D:` the lowest taken (`A:` and `B:` are the floppy drives' by
/// Windows' own convention, `C:` the system's); `held` is `GetLogicalDrives`' mask, bit 0 `A:`.
pub fn free_letter(held: u32) -> Option<char> {
    ('D'..='Z').rev().find(|letter| {
        let bit = u32::from(*letter).checked_sub(u32::from('A')).and_then(|n| 1u32.checked_shl(n)).unwrap_or(0);
        bit != 0 && held & bit == 0
    })
}

/// Where workspace `name` may be mounted, in the order to try: `root/<name>` alone (Unix); on Windows each drive letter
/// free now, the highest first ([`free_letter`]), so a letter another start took in the meantime gives way to the next.
pub fn points(root: &Path, name: &str) -> Vec<PathBuf> {
    if cfg!(windows) {
        let mut held = drives();
        let mut free = Vec::new();
        while let Some(letter) = free_letter(held) {
            free.push(PathBuf::from(format!("{letter}:")));
            let bit = u32::from(letter).checked_sub(u32::from('A')).and_then(|n| 1u32.checked_shl(n)).unwrap_or(0);
            if bit == 0 {
                break;
            }
            held |= bit;
        }
        free
    } else {
        vec![root.join(name)]
    }
}

/// Whether `point` is a drive letter some drive holds now (another start's mount, or any drive that came meanwhile);
/// never off Windows, where a workspace's folder is its own.
pub fn taken(point: &Path) -> bool {
    let text = point.to_string_lossy();
    let mut chars = text.chars();
    match (chars.next(), chars.next(), chars.next()) {
        (Some(letter), Some(':'), None) if cfg!(windows) && letter.is_ascii_alphabetic() => {
            let bit = u32::from(letter.to_ascii_uppercase()).checked_sub(u32::from('A')).and_then(|n| 1u32.checked_shl(n)).unwrap_or(0);
            drives() & bit != 0
        }
        _ => false,
    }
}

/// The folder at the top of a volume mounted at `point`: the point itself, or a drive's root (`K:` is `K:\`, since a
/// bare `K:` names the folder Windows last worked in on that drive, and `K:` joined with `src` is `K:src`).
pub fn top(point: &str) -> PathBuf {
    let mut chars = point.chars();
    match (chars.next(), chars.next(), chars.next()) {
        (Some(letter), Some(':'), None) if letter.is_ascii_alphabetic() => PathBuf::from(format!("{point}\\")),
        _ => PathBuf::from(point),
    }
}

/// Why [`points`] gave none: every drive letter is taken (Windows).
pub const NO_POINT: &str = "every drive letter is taken: Slates mounts a volume on Windows at a drive letter of its own, so a Windows machine holds as many workspaces at once as it has letters free";

/// Makes `point` ready to mount at: its folder made (Unix); nothing on Windows, where the drive appears with the mount.
pub fn ready(point: &Path) -> anyhow::Result<()> {
    if !cfg!(windows) {
        std::fs::create_dir_all(point)?;
    }
    Ok(())
}

/// What a mount leaves once it's gone: its empty folder removed (Unix); nothing on Windows, where the drive goes with
/// the mount.
pub fn clear(point: &Path) {
    if !cfg!(windows) {
        let _ = std::fs::remove_dir(point);
    }
}

/// Whether a volume is mounted at `point`: its device differs from its parent's.
#[cfg(unix)]
pub fn is_mount(point: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    let (Ok(d), Some(Ok(p))) = (std::fs::metadata(point), point.parent().map(std::fs::metadata)) else { return false };
    d.dev() != p.dev()
}

/// Whether a volume is mounted at `point`: the drive is there and Windows names its file system Slates'.
#[cfg(windows)]
pub fn is_mount(point: &Path) -> bool {
    windows::file_system(point).is_some_and(|fs| fs == windows::SLATES_FILE_SYSTEM)
}

#[cfg(not(any(unix, windows)))]
pub fn is_mount(_point: &Path) -> bool {
    false
}

/// The drives Windows holds now (`GetLogicalDrives`), none elsewhere.
fn drives() -> u32 {
    #[cfg(windows)]
    {
        windows::logical_drives()
    }
    #[cfg(not(windows))]
    {
        0
    }
}

/// Whether WinFsp's library is on this machine, where Slates looks for it (by name on the loader's path, else under
/// the directory WinFsp's installer records), without loading it.
fn winfsp_installed() -> bool {
    #[cfg(windows)]
    {
        windows::winfsp_installed()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt as _;
    use std::path::Path;

    /// The file system name Slates' WinFsp volume gives Windows (its `crates/bridge-winfsp/src/host.rs`,
    /// `file_system_name`).
    pub(super) const SLATES_FILE_SYSTEM: &str = "slates";
    /// WinFsp's library for this machine's architecture (`FSP_DLLNAME`), as Slates loads it.
    #[cfg(target_arch = "aarch64")]
    const LIBRARY: &str = "winfsp-a64.dll";
    /// WinFsp's library for this machine's architecture (`FSP_DLLNAME`), as Slates loads it.
    #[cfg(not(target_arch = "aarch64"))]
    const LIBRARY: &str = "winfsp-x64.dll";
    /// The registry key WinFsp's (32-bit) installer records its directory under on a 64-bit Windows, and its value.
    const REGISTRY_KEY: &str = "Software\\WOW6432Node\\WinFsp";
    const REGISTRY_VALUE: &str = "InstallDir";
    /// `HKEY_LOCAL_MACHINE`, `RRF_RT_REG_SZ`, and `ERROR_SUCCESS`.
    const HKEY_LOCAL_MACHINE: isize = 0x8000_0002_u32 as i32 as isize;
    const RRF_RT_REG_SZ: u32 = 0x0000_0002;
    const ERROR_SUCCESS: i32 = 0;
    /// The most UTF-16 units a path or a file system's name is read in here (`MAX_PATH`).
    const MAX_PATH: usize = 260;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetLogicalDrives() -> u32;
        fn GetVolumeInformationW(root: *const u16, name: *mut u16, name_len: u32, serial: *mut u32, max_component: *mut u32, flags: *mut u32, fs: *mut u16, fs_len: u32) -> i32;
        fn SearchPathW(path: *const u16, file: *const u16, extension: *const u16, len: u32, buffer: *mut u16, part: *mut *mut u16) -> u32;
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn RegGetValueW(key: isize, sub_key: *const u16, value: *const u16, flags: u32, kind: *mut u32, data: *mut c_void, len: *mut u32) -> i32;
    }

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    fn from_wide(units: &[u16]) -> String {
        let end = units.iter().position(|u| *u == 0).unwrap_or(units.len());
        String::from_utf16_lossy(units.get(..end).unwrap_or_default())
    }

    pub(super) fn logical_drives() -> u32 {
        // SAFETY: no arguments; it returns a mask (0 when it fails, which holds no drive).
        unsafe { GetLogicalDrives() }
    }

    /// The file system Windows names the drive `point` (`K:`) holds, when it holds one.
    pub(super) fn file_system(point: &Path) -> Option<String> {
        let mut root = point.as_os_str().to_os_string();
        root.push("\\");
        let root = wide(&root);
        let mut fs = [0u16; MAX_PATH];
        // SAFETY: a NUL-terminated root ending in a backslash, and the file system name's buffer with its length; the
        // rest not asked.
        let ok = unsafe { GetVolumeInformationW(root.as_ptr(), std::ptr::null_mut(), 0, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), fs.as_mut_ptr(), u32::try_from(fs.len()).ok()?) };
        (ok != 0).then(|| from_wide(&fs))
    }

    /// Whether WinFsp's library is where Slates' own search finds it (WinFsp's `FspLoad`): by name on the loader's
    /// search path, else in `<InstallDir>\bin\` as WinFsp's installer recorded it.
    pub(super) fn winfsp_installed() -> bool {
        let name = wide(std::ffi::OsStr::new(LIBRARY));
        let mut found = [0u16; MAX_PATH];
        let Ok(len) = u32::try_from(found.len()) else { return false };
        // SAFETY: the default search path (null), a NUL-terminated file name with no extension to add, and a buffer of
        // its stated length; the file part is not asked.
        let on_path = unsafe { SearchPathW(std::ptr::null(), name.as_ptr(), std::ptr::null(), len, found.as_mut_ptr(), std::ptr::null_mut()) };
        if on_path != 0 {
            return true;
        }
        let (key, value) = (wide(std::ffi::OsStr::new(REGISTRY_KEY)), wide(std::ffi::OsStr::new(REGISTRY_VALUE)));
        let mut dir = [0u16; MAX_PATH];
        let Ok(mut bytes) = u32::try_from(dir.len().saturating_mul(2)) else { return false };
        // SAFETY: a predefined key, NUL-terminated key and value names, a string asked for, and a buffer whose length
        // in bytes is given.
        let read = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), value.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), dir.as_mut_ptr().cast(), &raw mut bytes) };
        if read != ERROR_SUCCESS {
            return false;
        }
        let library = Path::new(&from_wide(&dir)).join("bin").join(LIBRARY);
        library.is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Windows' drives as a mask (bit 0 `A:`).
    fn mask(letters: &str) -> u32 {
        letters.chars().filter_map(|c| u32::from(c).checked_sub(u32::from('A')).and_then(|n| 1u32.checked_shl(n))).fold(0, |m, b| m | b)
    }

    /// Kiln goal 01a1ec34599974448aed2672bafaf9a2 (Windows): a workspace takes the highest drive letter free, never
    /// `A:` to `C:`, and none when every letter is taken.
    #[test]
    fn a_windows_workspace_takes_the_highest_drive_letter_free() {
        assert_eq!(free_letter(mask("C")), Some('Z'));
        assert_eq!(free_letter(mask("CXYZ")), Some('W'));
        assert_eq!(free_letter(mask("CDEFGHIJKLMNOPQRSTUVWXYZ")), None, "every letter from D: taken");
        assert_eq!(free_letter(mask("CEFGHIJKLMNOPQRSTUVWXYZ")), Some('D'));
        assert_eq!(free_letter(0), Some('Z'));
        assert_eq!(free_letter(u32::MAX), None);
    }

    /// A drive's workspace starts at the drive's root, never at the folder Windows last used on it; a folder's at itself.
    #[test]
    fn a_workspaces_top_is_its_drives_root_or_its_folder() {
        assert_eq!(top("K:"), PathBuf::from("K:\\"));
        assert_eq!(top("/state/workspaces/kiln-a"), PathBuf::from("/state/workspaces/kiln-a"));
    }

    /// Off Windows a workspace is mounted at its own folder under the workspaces folder.
    #[test]
    #[cfg(not(windows))]
    fn elsewhere_a_workspace_is_mounted_at_a_folder_of_its_own() {
        assert_eq!(points(Path::new("/state/workspaces"), "kiln-a"), vec![PathBuf::from("/state/workspaces/kiln-a")]);
    }
}
