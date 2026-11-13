//! `hex selftest-box escape <mode> <marker>`: the root-escape attempts a container may try, run inside a
//! box so the pruned-seed root's security criteria are tested through the real path (goal 5 plan step 2;
//! the research probe `rootprobe.c`, findings f66-f80). Each mode is one attempt, run in its own box as
//! root with `CAP_SYS_ADMIN` and `CAP_SYS_CHROOT`; it prints a line and exits 0 when the box held (the
//! secure outcome) and non-zero when it escaped or the expected refusal didn't happen. `marker` is a path
//! that exists on the host, outside any rootfs: if the box can open it, it has reached the host.
//!
//! - `e0` make a user namespace: expect allowed — a pruned container's root is the namespace's topmost
//!   mount, so it is not chrooted and may nest a user namespace (`current_chrooted`, f66).
//! - `e1` the classic chroot escape (`mkdir x; chroot x`, `..` until a directory is its own parent; `chroot .`):
//!   expect the root stays the rootfs, the marker unreachable (`..` can't climb a stack of root mounts, f67).
//! - `e2` unmount the root, then `setns` into the box's own mount namespace, which roots the caller at the
//!   namespace's topmost mount: expect only the seed's read-only tmpfs beneath, holding nothing, the
//!   marker unreachable (f69, f78) — where a root in a copy of the host's namespace would reach the host.
//! - `e3` mask `/proc/kcore`, then mount a fresh proc in a nested user/mount/pid namespace: expect it
//!   refused, the container's proc not being fully visible and the pruned namespace holding no other
//!   (`mnt_already_visible`, f68).
//!
//! Only safe calls: `rustix` and [`super::sys`]'s wrappers; no `unsafe` here. The process is
//! single-threaded (`hex` handles `selftest-box` before it starts anything), so `e3`'s fork is sound.

use std::os::fd::AsFd;

use rustix::fs::{Mode, OFlags};
use rustix::mount::{MountFlags, UnmountFlags};
use rustix::process::PidfdFlags;
use rustix::thread::UnshareFlags;

use crate::linux::oci::sys;

/// Writes one line to stdout with `write(2)` (no buffering or lock, so it is sound after `e3`'s fork).
fn say(msg: &str) {
    let line = format!("{msg}\n");
    let _ = rustix::io::write(std::io::stdout(), line.as_bytes());
}

/// Whether `marker` — a host path outside any rootfs — can be opened, meaning the box has reached the host.
fn reachable(marker: &str) -> bool {
    rustix::fs::open(marker, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty()).is_ok()
}

/// The most names of `/` that `e2` reports: a report's width, not a bound the box's security rests on. One
/// name is enough to fail it; the rest only say what was there, and a listing is cut at this many (§2).
const BENEATH_NAMES: usize = 8;

/// The names `/` holds, at most [`BENEATH_NAMES`] of them, for `e2` after it unmounted the root.
fn beneath() -> std::io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir("/")?.take(BENEATH_NAMES) {
        names.push(entry?.file_name().to_string_lossy().into_owned());
    }
    Ok(names)
}

/// How `e1` opens the directories it climbs through: by descriptor alone.
const DIR: OFlags = OFlags::PATH.union(OFlags::DIRECTORY).union(OFlags::CLOEXEC);

/// Most `..` `e1` climbs: no directory a path can name lies deeper than this, a path being at most `PATH_MAX`
/// (4,096) bytes with its NUL (include/uapi/linux/limits.h:13) and each component at least two of them (`/x`).
const MAX_CLIMB: usize = 4096 / 2;

/// `e1`'s climb: `..` from `from` until a directory is its own parent, as `/` is and as the root of a chroot
/// that holds is, at most [`MAX_CLIMB`] times; past that, `ELOOP`.
fn climb(from: std::os::fd::OwnedFd) -> rustix::io::Result<std::os::fd::OwnedFd> {
    let mut at = from;
    for _ in 0..=MAX_CLIMB {
        let up = rustix::fs::openat(&at, "..", DIR, Mode::empty())?;
        let (here, there) = (rustix::fs::fstat(&at)?, rustix::fs::fstat(&up)?);
        if (here.st_dev, here.st_ino) == (there.st_dev, there.st_ino) {
            return Ok(at);
        }
        at = up;
    }
    Err(rustix::io::Errno::LOOP)
}

/// Runs the escape `mode` against `marker`. Returns the process exit code: 0 when the box held (secure),
/// 1 when it escaped or an expected refusal didn't happen, 2 when the attempt couldn't be set up.
pub fn attempt(mode: &str, marker: &str) -> u8 {
    match mode {
        "e0" => match sys::unshare(UnshareFlags::NEWUSER) {
            Ok(()) => {
                say("E0 make a user namespace: allowed (secure)");
                0
            }
            Err(e) => {
                say(&format!(
                    "E0 make a user namespace: REFUSED ({}) (insecure)",
                    super::OciError::from(e)
                ));
                1
            }
        },
        "e1" => {
            let _ = rustix::fs::mkdir("/esc", Mode::from_bits_truncate(0o755));
            if let Err(e) = rustix::process::chroot("/esc") {
                say(&format!("E1 chroot escape: chroot /esc refused ({e})"));
                return 2;
            }
            let top = rustix::fs::open(".", DIR, Mode::empty()).and_then(climb);
            if let Ok(top) = top {
                let _ = rustix::process::fchdir(&top);
            }
            let _ = rustix::process::chroot(".");
            if reachable(marker) {
                say("E1 chroot escape: marker REACHED (insecure)");
                1
            } else {
                say("E1 chroot escape: marker not reached (secure)");
                0
            }
        }
        "e2" => {
            let pidfd =
                match rustix::process::pidfd_open(rustix::process::getpid(), PidfdFlags::empty()) {
                    Ok(fd) => fd,
                    Err(e) => {
                        say(&format!("E2 umount / then setns: pidfd_open ({e})"));
                        return 2;
                    }
                };
            if let Err(e) = rustix::mount::unmount("/", UnmountFlags::DETACH) {
                // The root couldn't be unmounted (a locked root in a user namespace): the box held.
                say(&format!(
                    "E2 umount / then setns: umount refused ({e}) (secure)"
                ));
                return 0;
            }
            if let Err(e) = sys::setns(pidfd.as_fd(), libc::CLONE_NEWNS) {
                say(&format!(
                    "E2 umount / then setns: setns refused ({}) (secure)",
                    super::OciError::from(e)
                ));
                return 0;
            }
            if reachable(marker) {
                say("E2 umount / then setns: marker REACHED (insecure)");
                return 1;
            }
            // Beneath the container's root lies only the seed's tmpfs, which holds nothing: a name there
            // is something beneath the root that a box could reach.
            match beneath() {
                Ok(names) if names.is_empty() => {
                    say("E2 umount / then setns: marker not reached, / holds nothing (secure)");
                    0
                }
                Ok(names) => {
                    say(&format!(
                        "E2 umount / then setns: / holds {names:?} beneath the root (insecure)"
                    ));
                    1
                }
                Err(e) => {
                    say(&format!("E2 umount / then setns: listing / failed ({e})"));
                    2
                }
            }
        }
        "e3" => {
            // Mask /proc/kcore (as a spec's maskedPaths does), so the container's proc isn't fully visible.
            if let Err(e) = rustix::mount::mount_bind("/dev/null", "/proc/kcore") {
                say(&format!(
                    "E3 nested proc mount: masking /proc/kcore failed ({e})"
                ));
                return 2;
            }
            if let Err(e) =
                sys::unshare(UnshareFlags::NEWUSER | UnshareFlags::NEWNS | UnshareFlags::NEWPID)
            {
                say(&format!(
                    "E3 nested proc mount: unshare failed ({})",
                    super::OciError::from(e)
                ));
                return 2;
            }
            // Become root in the new user namespace, so the mount is tried with privilege.
            let map = |path: &str, text: &str| {
                let _ = rustix::fs::open(path, OFlags::WRONLY | OFlags::CLOEXEC, Mode::empty())
                    .and_then(|fd| rustix::io::write(&fd, text.as_bytes()).map(drop));
            };
            map("/proc/self/setgroups", "deny");
            map("/proc/self/uid_map", "0 0 1");
            map("/proc/self/gid_map", "0 0 1");
            // The caller joins the new pid namespace only through a child, which is its PID 1.
            match sys::fork() {
                Ok(sys::Fork::Child) => {
                    let _ = rustix::fs::mkdir("/tmp/np", Mode::from_bits_truncate(0o755));
                    let code = match rustix::mount::mount(
                        "proc",
                        "/tmp/np",
                        "proc",
                        MountFlags::empty(),
                        None,
                    ) {
                        Ok(()) => {
                            say("E3 nested proc mount: ALLOWED (insecure)");
                            1
                        }
                        Err(e) => {
                            say(&format!("E3 nested proc mount: refused ({e}) (secure)"));
                            0
                        }
                    };
                    sys::exit_child(code);
                }
                Ok(sys::Fork::Parent(pid)) => match sys::waitpid(pid) {
                    Ok(st) => u8::try_from(st.exit_code().unwrap_or(2)).unwrap_or(2),
                    Err(e) => {
                        say(&format!(
                            "E3 nested proc mount: reaping the child failed ({e})"
                        ));
                        2
                    }
                },
                Err(e) => {
                    say(&format!("E3 nested proc mount: fork failed ({e})"));
                    2
                }
            }
        }
        other => {
            say(&format!("escape: unknown mode {other:?}"));
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal 5 plan step 2 (T-T7.1.4), E1's climb under Ada's 2026-11-06 rule: do climb from a directory 24 below a
    /// temporary one, unconfined; expect it to reach `/` (the directory that is its own parent), however deep it
    /// started, where a fixed count of climbs stopped short of it.
    #[test]
    fn the_climb_goes_up_until_a_directory_is_its_own_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let deep = (0..24).fold(tmp.path().to_path_buf(), |p, _| p.join("d"));
        std::fs::create_dir_all(&deep).unwrap();
        let top = climb(rustix::fs::open(&deep, DIR, Mode::empty()).unwrap()).unwrap();
        let (top, root) = (
            rustix::fs::fstat(&top).unwrap(),
            rustix::fs::stat("/").unwrap(),
        );
        assert_eq!((top.st_dev, top.st_ino), (root.st_dev, root.st_ino));
    }
}
