//! Removing a folder and all it holds without going into a file system mounted inside it.
//!
//! `std::fs::remove_dir_all` opens every folder it meets, a mount point too. A FUSE mount a test
//! left in a leg's workspace (`target/tmp/…/mnt`) whose server is gone, its connection still
//! open, answers that open never: the thread removing the workspace waits for ever, and with it
//! the leg's start (`workspaces::claim` sweeps what earlier legs left) and end (W06-R01; adacachy2,
//! 2026-11-18: every leg's start and end held, the runner silent for a day).
//!
//! So a mount under the folder is never opened, nor stat'd: the mounts are read from
//! `/proc/self/mountinfo` (Linux) and matched by path alone. A FUSE mount is let go of first,
//! lazily (`fusermount3 -u -z`: no request goes to its server, so a gone or hung one holds
//! nothing up), and its folder removed with the rest; any other mount is left where it is, with
//! the folders holding it, and the error names it. Elsewhere on Unix a folder on another device
//! than the one removed is left alike; Windows removes as `std` does.

use std::path::{Path, PathBuf};

/// Removes `root` and all it holds, never going into a file system mounted under it (the module's
/// doc). A mount that couldn't be let go of is left, with the folders holding it: an error naming
/// it, after all else went.
pub fn remove_dir_all(root: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        std::fs::remove_dir_all(root)
    }
    #[cfg(not(windows))]
    {
        let meta = std::fs::symlink_metadata(root)?;
        if !meta.is_dir() {
            return std::fs::remove_file(root);
        }
        let skip = let_go_of(root);
        let mut left = Vec::new();
        remove_within(root, &skip, dev(&meta), &mut left)?;
        if left.is_empty() {
            return std::fs::remove_dir(root);
        }
        Err(std::io::Error::other(format!(
            "{} left: {} mounted there",
            root.display(),
            left.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
        )))
    }
}

/// The mount points at or under `root`, as paths under `root` as it was given (Linux; none
/// elsewhere), the shallowest first, each with its file system's type.
pub fn mounts_under(root: &Path) -> Vec<(PathBuf, String)> {
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
        under(root, &std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()), &text)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = root;
        Vec::new()
    }
}

/// [`mounts_under`] of `mountinfo`'s text, for `root` whose real path is `real`.
#[cfg(target_os = "linux")]
fn under(root: &Path, real: &Path, mountinfo: &str) -> Vec<(PathBuf, String)> {
    let mut found: Vec<(PathBuf, String)> = super::sense_disk_linux::mountinfo(mountinfo)
        .into_iter()
        .filter_map(|m| {
            let rest = Path::new(&m.mount).strip_prefix(real).ok()?.to_path_buf();
            Some((if rest.as_os_str().is_empty() { root.to_path_buf() } else { root.join(rest) }, m.fs))
        })
        .collect();
    found.sort_by_key(|(p, _)| p.components().count());
    found.dedup_by(|a, b| a.0 == b.0);
    found
}

/// Lets go of the FUSE mounts under `root` (the shallowest first: one under another goes with it),
/// and says the mounts left there, which are never to be opened.
#[cfg(not(windows))]
fn let_go_of(root: &Path) -> Vec<PathBuf> {
    let mounts = mounts_under(root);
    if mounts.is_empty() {
        return Vec::new();
    }
    let mut gone: Vec<&Path> = Vec::new();
    for (p, fs) in &mounts {
        if gone.iter().any(|g| p.starts_with(g)) || !is_fuse(fs) {
            continue;
        }
        if detach_fuse(p) {
            tracing::info!(mount = %p.display(), fs = %fs, "a FUSE mount left in a folder being removed was let go of");
            gone.push(p);
        }
    }
    if gone.is_empty() {
        return mounts.into_iter().map(|(p, _)| p).collect();
    }
    mounts_under(root).into_iter().map(|(p, _)| p).collect()
}

/// A FUSE file system's type (`fuse`, `fuseblk`, `fuse.<name>`). Where [`let_go_of`] is: Windows has no FUSE mounts to let
/// go of, and the function unused there failed a Windows build under `-D warnings`.
#[cfg(not(windows))]
fn is_fuse(fs: &str) -> bool {
    fs == "fuse" || fs == "fuseblk" || fs.starts_with("fuse.")
}

/// Lazily unmounts FUSE mount `p` as its owner may (`fusermount3 -u -z`, else `fusermount`):
/// whether it went.
#[cfg(not(windows))]
fn detach_fuse(p: &Path) -> bool {
    ["fusermount3", "fusermount"].iter().any(|bin| {
        std::process::Command::new(bin)
            .args(["-u", "-z", "--"])
            .arg(p)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

#[cfg(unix)]
fn dev(m: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    m.dev()
}

#[cfg(not(any(unix, windows)))]
fn dev(_: &std::fs::Metadata) -> u64 {
    0
}

/// Removes what `dir` holds, `skip`'s paths and folders on a device other than `device` left
/// (pushed to `left`), and each folder under it emptied. Nothing in `skip` is opened or stat'd:
/// an entry's kind is the one its folder's listing gives.
#[cfg(not(windows))]
fn remove_within(dir: &Path, skip: &[PathBuf], device: u64, left: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let path = e.path();
        if skip.contains(&path) {
            left.push(path);
            continue;
        }
        if !e.file_type()?.is_dir() {
            std::fs::remove_file(&path)?;
            continue;
        }
        if dev(&e.metadata()?) != device {
            left.push(path);
            continue;
        }
        let before = left.len();
        remove_within(&path, skip, device, left)?;
        if left.len() == before {
            std::fs::remove_dir(&path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w06_r01_a_tree_goes_whole_with_nothing_mounted_in_it() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("ws");
        std::fs::create_dir_all(root.join("a/b/c"))?;
        std::fs::write(root.join("a/b/c/f"), "x")?;
        std::fs::write(root.join("top"), "x")?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.path(), root.join("a/link"))?;
        remove_dir_all(&root)?;
        assert!(!root.exists());
        assert!(dir.path().exists(), "a symlink's target is never followed");
        assert_eq!(remove_dir_all(&root).map_err(|e| e.kind()), Err(std::io::ErrorKind::NotFound));
        Ok(())
    }

    /// A path in `skip` (a mount point) is never opened: here one no one may list, which
    /// `std::fs::remove_dir_all` would fail on, stays, with the folders holding it, and all else
    /// goes.
    #[cfg(unix)]
    #[test]
    fn w06_r01_a_mount_point_in_the_tree_is_never_gone_into() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("ws");
        let mnt = root.join("target/tmp/slates-chaos-1/mnt");
        std::fs::create_dir_all(mnt.join("inside"))?;
        std::fs::create_dir_all(root.join("target/debug"))?;
        std::fs::write(root.join("target/debug/bin"), "x")?;
        std::fs::write(root.join("target/tmp/slates-chaos-1/log"), "x")?;
        std::fs::set_permissions(&mnt, std::fs::Permissions::from_mode(0o000))?;
        let meta = std::fs::symlink_metadata(&root)?;
        let mut left = Vec::new();
        let r = remove_within(&root, std::slice::from_ref(&mnt), dev(&meta), &mut left);
        std::fs::set_permissions(&mnt, std::fs::Permissions::from_mode(0o755))?;
        r?;
        assert_eq!(left, vec![mnt.clone()]);
        assert!(mnt.join("inside").exists(), "what is mounted there is left as it is");
        assert!(!root.join("target/debug").exists() && !root.join("target/tmp/slates-chaos-1/log").exists());
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn w06_r01_the_mounts_under_a_folder_are_found_by_path_alone() {
        let info = "\
22 1 0:23 / / rw - btrfs /dev/nvme1n1p2 rw
98 22 0:98 / /w/ws/target/tmp/slates-owner-turn-1/mnt rw,nosuid - fuse.slates slates rw
99 98 0:99 / /w/ws/target/tmp/slates-owner-turn-1/mnt/deeper rw - tmpfs tmpfs rw
100 22 0:100 / /w/ws2/target/tmp/x/mnt rw - fuse.slates slates rw
101 22 0:101 / /w/ws/with\\040space rw - nfs host:/e rw
";
        let found = under(Path::new("/link/ws"), Path::new("/w/ws"), info);
        assert_eq!(
            found,
            vec![
                (PathBuf::from("/link/ws/with space"), "nfs".to_owned()),
                (PathBuf::from("/link/ws/target/tmp/slates-owner-turn-1/mnt"), "fuse.slates".to_owned()),
                (PathBuf::from("/link/ws/target/tmp/slates-owner-turn-1/mnt/deeper"), "tmpfs".to_owned()),
            ],
            "the shallowest first, as the folder was named; a sibling folder sharing its name's start isn't under it"
        );
        assert_eq!(under(Path::new("/w/ws"), Path::new("/w/ws"), "98 22 0:98 / /w/ws rw - fuse.slates slates rw\n"), vec![(PathBuf::from("/w/ws"), "fuse.slates".to_owned())]);
        assert!(is_fuse("fuse.slates") && is_fuse("fuse") && is_fuse("fuseblk") && !is_fuse("fusectl") && !is_fuse("nfs"));
    }
}
