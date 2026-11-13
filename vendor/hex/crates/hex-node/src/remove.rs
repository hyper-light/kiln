//! Removing a box's directory however its container left it. A box's writable layer holds what the
//! container wrote: a tree deeper than any stack, and directories it made unreadable, overlayfs's own
//! work directory among them (made mode 0, fs/overlayfs/super.c `ovl_workdir_create`). So the tree is
//! taken apart flat, with no recursion (CLAUDE.md §11) and two descriptors open at most, however deep
//! it is: each directory's entries are moved up into the top one, then the directory, now empty, is
//! removed. Every entry moves at most once, and nothing adds entries while it runs: the box's
//! processes are gone before its directories are removed.

use std::ffi::CStr;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;
use std::time::{Duration, Instant};

use hex_api::Refusal;
use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, RenameFlags};
use rustix::io::Errno;

/// The name an entry moved up into the top directory takes: a prefix and a count no other entry
/// there has, since every entry that reaches the top is renamed on the way.
const MOVED: &str = ".hex-rm-";

/// Removes `dir` and everything under it; absent already is removed. Past `patience` it stops and
/// refuses, typed, leaving the rest for a later call to finish, which starts where this one stopped.
pub fn remove_tree(dir: &Path, patience: Duration) -> Result<(), Refusal> {
    let top = match open_dir(rustix::fs::CWD, dir) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Ok(()),
        Err(e) => return Err(failed(dir, e)),
    };
    let deadline = Instant::now().checked_add(patience);
    let mut moved: u64 = 0;
    loop {
        let mut pass = Dir::read_from(&top).map_err(|e| failed(dir, e))?;
        let mut empty = true;
        while let Some(entry) = pass.read() {
            let entry = entry.map_err(|e| failed(dir, e))?;
            let name = entry.file_name();
            if name == c"." || name == c".." {
                continue;
            }
            empty = false;
            if deadline.is_none_or(|d| Instant::now() >= d) {
                return Err(Refusal::Os {
                    op: "removing a box's directory",
                    why: format!(
                        "{} still holds entries after {patience:?}; a later delete goes on from here",
                        dir.display()
                    ),
                });
            }
            if kind(top.as_fd(), &entry).map_err(|e| failed(dir, e))? == FileType::Directory {
                lift(&top, name, &mut moved).map_err(|e| failed(dir, e))?;
            } else {
                rustix::fs::unlinkat(&top, name, AtFlags::empty()).map_err(|e| failed(dir, e))?;
            }
        }
        if empty {
            break;
        }
    }
    drop(top);
    rustix::fs::unlinkat(rustix::fs::CWD, dir, AtFlags::REMOVEDIR).map_err(|e| failed(dir, e))
}

/// Moves each entry of `name`, a directory in `top`, up into `top` under a name of its own, then
/// removes `name`, now empty. Each directory is first made its owner's to read, search and write,
/// whatever mode the container gave it: `name` to be read and emptied, and each directory in it to be
/// moved, since a directory moves with its `..`, which rename(2) needs it writable to change. The
/// node owns everything in a rootless box's layer, and root needs no such leave.
fn lift(top: &OwnedFd, name: &CStr, moved: &mut u64) -> Result<(), Errno> {
    rustix::fs::chmodat(top, name, Mode::RWXU, AtFlags::empty())?;
    let mut entries = Dir::new(open_dir(top.as_fd(), name)?)?;
    while let Some(entry) = entries.read() {
        let entry = entry?;
        let child = entry.file_name();
        if child == c"." || child == c".." {
            continue;
        }
        let from = entries.fd()?;
        if kind(from, &entry)? == FileType::Directory {
            rustix::fs::chmodat(from, child, Mode::RWXU, AtFlags::empty())?;
        }
        loop {
            *moved = moved.checked_add(1).ok_or(Errno::OVERFLOW)?;
            let to = format!("{MOVED}{moved}");
            match rustix::fs::renameat_with(from, child, top, to.as_str(), RenameFlags::NOREPLACE) {
                Err(Errno::EXIST) => {}
                done => break done?,
            }
        }
    }
    rustix::fs::unlinkat(top, name, AtFlags::REMOVEDIR)
}

/// What an entry of the directory `at` is, from the entry where its filesystem says, else from a
/// look at it that doesn't follow a symlink.
fn kind(at: BorrowedFd<'_>, entry: &rustix::fs::DirEntry) -> Result<FileType, Errno> {
    match entry.file_type() {
        FileType::Unknown => rustix::fs::statat(at, entry.file_name(), AtFlags::SYMLINK_NOFOLLOW)
            .map(|st| FileType::from_raw_mode(st.st_mode)),
        known => Ok(known),
    }
}

/// A directory opened to read, never through a symlink.
fn open_dir<P: rustix::path::Arg>(at: BorrowedFd<'_>, path: P) -> Result<OwnedFd, Errno> {
    rustix::fs::openat(
        at,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
}

fn failed(dir: &Path, e: Errno) -> Refusal {
    Refusal::Os {
        op: "removing a box's directory",
        why: format!("{}: {e}", dir.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    /// CLAUDE.md §11 and T-T7.1.2: a tree 20,000 directories deep, deeper than a recursive walk's
    /// descriptors or a path's 4,096 bytes reach, comes apart, with what's beside it untouched.
    #[test]
    fn a_tree_deeper_than_any_path_is_removed() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("box");
        std::fs::create_dir(&dir).unwrap();
        let mut at = open_dir(rustix::fs::CWD, &dir).unwrap();
        for _ in 0..20_000 {
            rustix::fs::mkdirat(&at, "d", Mode::RWXU).unwrap();
            at = open_dir(at.as_fd(), "d").unwrap();
        }
        rustix::fs::openat(&at, "leaf", OFlags::CREATE | OFlags::WRONLY, Mode::RUSR).unwrap();
        drop(at);
        std::fs::write(base.path().join("beside"), b"kept").unwrap();
        remove_tree(&dir, Duration::from_secs(60)).unwrap();
        assert!(!dir.exists());
        assert_eq!(std::fs::read(base.path().join("beside")).unwrap(), b"kept");
    }

    /// A layer the container left as overlayfs and its programs leave it: a work directory of mode 0,
    /// an unreadable directory with files in it, a read-only one, a whiteout-like fifo, and a symlink
    /// to a directory outside, which is removed as a link and never followed.
    #[test]
    fn unreadable_directories_and_links_out_are_handled() {
        let base = tempfile::tempdir().unwrap();
        let outside = base.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("precious"), b"kept").unwrap();
        let dir = base.path().join("box");
        for d in ["work/work", "upper/locked/inner", "upper/readonly"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        std::fs::write(dir.join("upper/locked/inner/file"), b"x").unwrap();
        std::fs::write(dir.join("upper/readonly/file"), b"x").unwrap();
        symlink(&outside, dir.join("upper/escape")).unwrap();
        rustix::fs::mknodat(
            rustix::fs::CWD,
            dir.join("upper/fifo"),
            FileType::Fifo,
            Mode::RUSR,
            0,
        )
        .unwrap();
        for (d, mode) in [
            ("work/work", 0o000),
            ("upper/locked", 0o000),
            ("upper/readonly", 0o500),
            ("upper", 0o000),
        ] {
            std::fs::set_permissions(dir.join(d), std::fs::Permissions::from_mode(mode)).unwrap();
        }
        remove_tree(&dir, Duration::from_secs(60)).unwrap();
        assert!(!dir.exists());
        assert_eq!(std::fs::read(outside.join("precious")).unwrap(), b"kept");
    }

    /// Absent is removed; a symlink named as the directory is refused, not followed.
    #[test]
    fn absent_is_removed_and_a_link_is_not_followed() {
        let base = tempfile::tempdir().unwrap();
        remove_tree(&base.path().join("gone"), Duration::from_secs(1)).unwrap();
        let target = base.path().join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("file"), b"kept").unwrap();
        symlink(&target, base.path().join("link")).unwrap();
        assert!(remove_tree(&base.path().join("link"), Duration::from_secs(1)).is_err());
        assert_eq!(std::fs::read(target.join("file")).unwrap(), b"kept");
    }

    /// CLAUDE.md §2: past its patience a removal refuses, typed, and a later one finishes it.
    #[test]
    fn a_removal_out_of_patience_refuses_and_a_later_one_finishes() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("box");
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        assert!(matches!(
            remove_tree(&dir, Duration::ZERO),
            Err(Refusal::Os {
                op: "removing a box's directory",
                ..
            })
        ));
        assert!(dir.exists());
        remove_tree(&dir, Duration::from_secs(60)).unwrap();
        assert!(!dir.exists());
    }
}
