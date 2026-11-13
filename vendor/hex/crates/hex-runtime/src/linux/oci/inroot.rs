//! Paths in a container's root, resolved inside it. The container init sets its root up before it
//! switches into it, while `/` is still the host's (the classic path) or the pruned seed's (decision 5),
//! and an image's root may hold a link to anywhere: an ordinary walk of an absolute link lands outside,
//! where making a missing mount target writes. So every path the init touches in the root is opened
//! with `openat2(RESOLVE_IN_ROOT)` from one descriptor of the root ([`Root`]), which walks a link as
//! though the root were `/` (openat2(2)), and what's missing is made relative to the directory it
//! resolved to; a mount goes on the opened file, by its descriptor or through its `/proc/self/fd` link,
//! as runc mounts (libcontainer/rootfs_linux.go). A path that leads, through a link, to nothing inside
//! the root is refused by name, never followed out (CLAUDE.md §3).

use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::path::Path;

use rustix::fs::{Mode, OFlags, ResolveFlags};
use rustix::io::Errno;

use crate::linux::oci::OciError;

/// What a missing path is made as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Make {
    /// A directory, as a mount of a filesystem or a directory needs.
    Dir,
    /// An empty file, as a bind of a file needs.
    File,
    /// Nothing: a missing path is left missing.
    Nothing,
}

/// A container's root, held as one descriptor for every path the init resolves in it. A path that's
/// there resolves in one `openat2` from it, its links followed inside the root; only a path with
/// something missing is walked component by component, to make what's missing. The goal 5 plan step 3
/// trace found the init re-opening the root and re-resolving each prefix of every path it touched, 150
/// calls per create where the mounts need about 40 (`docs/research/runtime/goal5-mounts.md`).
pub struct Root<'a> {
    fd: BorrowedFd<'a>,
}

impl<'a> Root<'a> {
    /// The root `fd` is: a directory's descriptor, or the root of a mount tree (the pruned path's rootfs,
    /// cloned by the agent).
    pub fn new(fd: BorrowedFd<'a>) -> Self {
        Self { fd }
    }

    /// `dest`, a path in the container, opened (`O_PATH`) in the root, with what's missing made first
    /// as `make` says, its parents as directories; `None` when it's missing and `make` is
    /// [`Make::Nothing`]. A path that's there resolves in one call; any other walks the path component by
    /// component as before, so what's missing is made as before and a path refused is refused naming the
    /// component it was refused at, as before.
    pub fn open(&self, dest: &str, make: Make) -> Result<Option<OwnedFd>, OciError> {
        match in_root(self.fd, &normal(dest)) {
            Ok(fd) => return Ok(Some(fd)),
            // A component missing: nothing before it was refused (the kernel resolves in order), so
            // this is the walk's answer too.
            Err(Errno::NOENT) if make == Make::Nothing => return Ok(None),
            Err(_) => {}
        }
        self.walk(dest, make)
    }

    /// Optimization guard only: a prepared `/dev` may expose defaults earlier than ordinary setup.
    /// A path with any symlink is left to ordinary setup. NOENT is safe: all earlier components were
    /// ordinary and the mount creates the missing components. Errors are reproduced by `open` later.
    pub(crate) fn without_symlinks(&self, dest: &str) -> bool {
        match rustix::fs::openat2(
            self.fd,
            normal(dest),
            OFlags::PATH | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::IN_ROOT | ResolveFlags::NO_MAGICLINKS | ResolveFlags::NO_SYMLINKS,
        ) {
            Ok(_) | Err(Errno::NOENT) => true,
            Err(_) => false,
        }
    }

    /// `dest`'s parent directory opened in the root (made when missing) and `dest`'s last component,
    /// for a call that makes that component itself, relative to the directory (`mknodat`, `symlinkat`).
    pub fn parent(&self, dest: &str) -> Result<(OwnedFd, String), OciError> {
        let trimmed = dest.trim_end_matches('/');
        let (dir, name) = trimmed.rsplit_once('/').unwrap_or(("", trimmed));
        if name.is_empty() || name == "." || name == ".." {
            return Err(OciError::Config(format!("{dest:?} names no file")));
        }
        let dir = self
            .open(dir, Make::Dir)?
            .ok_or_else(|| OciError::Config(format!("{dest}: its directory can't be made")))?;
        Ok((dir, name.to_owned()))
    }

    /// `dest` walked component by component, each missing one made (a directory, or `make` for the
    /// last) relative to the directory its parent resolved to; `None` at the first missing one when
    /// `make` is [`Make::Nothing`].
    fn walk(&self, dest: &str, make: Make) -> Result<Option<OwnedFd>, OciError> {
        let parts: Vec<&str> = dest
            .split('/')
            .filter(|p| !p.is_empty() && *p != ".")
            .collect();
        let mut done = String::new();
        for (i, part) in parts.iter().enumerate() {
            let parent = if done.is_empty() {
                ".".to_owned()
            } else {
                done.clone()
            };
            if !done.is_empty() {
                done.push('/');
            }
            done.push_str(part);
            let last = i.checked_add(1) == Some(parts.len());
            match in_root(self.fd, &done) {
                Ok(fd) if last => return Ok(Some(fd)),
                Ok(_) => continue,
                Err(Errno::NOENT) => {}
                Err(e) => return Err(failed(dest, &done, e)),
            }
            if make == Make::Nothing {
                return Ok(None);
            }
            let dir = in_root(self.fd, &parent).map_err(|e| failed(dest, &parent, e))?;
            let made = match if last { make } else { Make::Dir } {
                Make::File => rustix::fs::openat(
                    &dir,
                    *part,
                    OFlags::CREATE
                        | OFlags::EXCL
                        | OFlags::NOFOLLOW
                        | OFlags::WRONLY
                        | OFlags::CLOEXEC,
                    Mode::from_bits_truncate(0o644),
                )
                .map(drop),
                Make::Dir | Make::Nothing => {
                    rustix::fs::mkdirat(&dir, *part, Mode::from_bits_truncate(0o755))
                }
            };
            match made {
                // Already there: a link, which the next open follows inside the root.
                Ok(()) | Err(Errno::EXIST) => {}
                Err(e) => return Err(failed(dest, &done, e)),
            }
            match in_root(self.fd, &done) {
                Ok(fd) if last => return Ok(Some(fd)),
                Ok(_) => {}
                Err(Errno::NOENT) => {
                    return Err(OciError::Config(format!(
                        "{dest}: {done} in the container's root is a link to nothing inside it"
                    )));
                }
                Err(e) => return Err(failed(dest, &done, e)),
            }
        }
        // No component: the root itself.
        in_root(self.fd, ".")
            .map(Some)
            .map_err(|e| failed(dest, ".", e))
    }
}

/// The container's root directory at `rootfs` opened (`O_PATH`), for a [`Root`] over it.
pub fn open_root(rootfs: &Path) -> Result<OwnedFd, OciError> {
    rustix::fs::open(
        rootfs,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| OciError::os("opening the container's root", e))
}

/// Whether `dest` names the root itself (`/`, or only empty and `.` components).
pub fn is_root(dest: &str) -> bool {
    dest.split('/').all(|p| p.is_empty() || p == ".")
}

/// The path a mount call takes to land on the file `fd` names: its `/proc/self/fd` link, which the
/// kernel follows to exactly that file, with no walk through the root's links again. It names the
/// file only while `fd` is open, so the caller holds `fd` until the call that uses it returns.
pub fn fd_path(fd: &OwnedFd) -> String {
    format!("/proc/self/fd/{}", fd.as_raw_fd())
}

/// `dest` as the root's walk reads it: its components but empty and `.` ones, `/`-joined, and `.` for
/// the root itself, so one `openat2` resolves exactly what the component walk would.
fn normal(dest: &str) -> String {
    let mut out = String::with_capacity(dest.len());
    for part in dest.split('/').filter(|p| !p.is_empty() && *p != ".") {
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(part);
    }
    if out.is_empty() {
        out.push('.');
    }
    out
}

/// How many times [`in_root`] tries a path Linux refuses with EAGAIN: a rename or a mount anywhere on the
/// host crossing a `..` in the walk, where it can't then say the `..` stayed in the root, and leaves the
/// retry to the caller (openat2(2); fs/namei.c v6.19 :2195-2206). Twice the most a resolve was measured to need:
/// on fedora under 16 mount and 16 rename loops, none needed more than 5 (unit hex-inroot-storm-20261105T130241Z,
/// 10 runs of 2,007 resolves; docs/research/runtime/inroot-eagain.md §3), and a failure bound takes a margin of 2
/// (Ada's 2026-11-06 rule). libpathrs's 128 (src/syscalls.rs:864-873) was copied, not measured here. A host that
/// needs more is refused by name ([`failed`]), the record to re-derive it from.
const IN_ROOT_TRIES: u32 = 2 * 5;

thread_local! {
    /// The EAGAIN refusals [`in_root`] tried again past, on this thread: the init's count, as it is
    /// single-threaded (a non-vacuity counter the tests read; the init has no channel to the agent's metrics).
    static RETRIED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The EAGAIN refusals [`in_root`] has tried again past on this thread.
pub fn retried() -> u64 {
    RETRIED.with(std::cell::Cell::get)
}

/// `path` opened in `root` as though `root` were `/`; magic links (`/proc/<pid>/root`) refused, as
/// openat2(2) advises asking for explicitly.
fn in_root(root: BorrowedFd<'_>, path: &str) -> Result<OwnedFd, Errno> {
    let open = || {
        rustix::fs::openat2(
            root,
            path,
            OFlags::PATH | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::IN_ROOT | ResolveFlags::NO_MAGICLINKS,
        )
    };
    tried(open)
}

/// `open` tried until it answers other than EAGAIN, at most [`IN_ROOT_TRIES`] times, each EAGAIN tried again
/// past counted ([`retried`]); past the budget, the last EAGAIN.
fn tried(mut open: impl FnMut() -> Result<OwnedFd, Errno>) -> Result<OwnedFd, Errno> {
    for _ in 1..IN_ROOT_TRIES {
        match open() {
            Err(Errno::AGAIN) => RETRIED.with(|n| n.set(n.get().saturating_add(1))),
            done => return done,
        }
    }
    open()
}

/// A path the resolver refused, naming it and the component it was refused at. An EAGAIN past
/// [`IN_ROOT_TRIES`] says so: Linux couldn't vouch for a `..` in any of the tries.
fn failed(dest: &str, at: &str, e: Errno) -> OciError {
    let why = if e == Errno::AGAIN {
        format!(
            "{dest} ({at}): Linux couldn't vouch for its `..` in {IN_ROOT_TRIES} tries, a rename or mount \
             elsewhere crossing each (EAGAIN)"
        )
    } else {
        format!("{dest} ({at}): {e}")
    };
    OciError::os("resolving a path in the container's root", why)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;
    use std::os::unix::fs::symlink;

    /// A root holding links out of it, absolute and relative, and one to a directory inside it.
    fn root_with_links(outside: &Path) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        symlink(outside, root.path().join("etc")).unwrap();
        symlink("../../../../..", root.path().join("up")).unwrap();
        std::fs::create_dir(root.path().join("real-etc")).unwrap();
        symlink("/real-etc", root.path().join("inside")).unwrap();
        root
    }

    /// An image's absolute link to a host directory: the path through it is refused by name, and
    /// nothing is made on the host.
    #[test]
    fn a_link_out_of_the_root_is_never_followed_out() {
        let host = tempfile::tempdir().unwrap();
        let root = root_with_links(host.path());
        let fd = open_root(root.path()).unwrap();
        let got = Root::new(fd.as_fd()).open("/etc/hosts", Make::File);
        assert!(
            matches!(&got, Err(OciError::Config(why)) if why.contains("a link to nothing inside it")),
            "{got:?}"
        );
        assert_eq!(std::fs::read_dir(host.path()).unwrap().count(), 0);
    }

    /// A rename anywhere on the host crossing a path's `..` makes Linux refuse the path with EAGAIN
    /// (openat2(2)); the resolver tries again within its budget, so a create isn't refused for what another
    /// process did (AC-T7.1.4). Do: resolve a link climbing out 2,000 times while another thread renames a
    /// file back and forth. Expect each to resolve.
    #[test]
    fn a_rename_elsewhere_doesnt_refuse_a_path_with_a_dotdot() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let host = tempfile::tempdir().unwrap();
        let root = root_with_links(host.path());
        let fd = open_root(root.path()).unwrap();
        let (a, b) = (host.path().join("a"), host.path().join("b"));
        std::fs::write(&a, b"").unwrap();
        let done = AtomicBool::new(false);
        let refused = std::thread::scope(|scope| {
            scope.spawn(|| {
                while !done.load(Ordering::Relaxed) {
                    std::fs::rename(&a, &b).unwrap();
                    std::fs::rename(&b, &a).unwrap();
                }
            });
            let refused = (0..2000)
                .find_map(|_| Root::new(fd.as_fd()).open("/up/made/here", Make::Dir).err());
            done.store(true, Ordering::Relaxed);
            refused
        });
        assert!(refused.is_none(), "{refused:?}");
    }

    /// A mount anywhere on the host, in any mount namespace, bumps the kernel's one `mount_lock` seqcount, and a
    /// scoped walk that crossed a `..` meanwhile is refused with EAGAIN (fs/namei.c v6.19 :2195-2206, :2637-2638;
    /// openat2(2)), as a node's own concurrent creates' mounts do to each other (AC-T7.1.4, CLAUDE.md §2, §4). Do:
    /// resolve a link climbing out at least 2,000 times while a process in a user and mount namespace of its own mounts
    /// and unmounts a tmpfs without pause, on until the resolver has tried again past an EAGAIN. Expect: each
    /// resolves, none refused for what the other process did, and the race was met (the retry counter rose).
    #[test]
    fn a_mount_elsewhere_doesnt_refuse_a_path_with_a_dotdot() {
        use std::io::BufRead as _;

        let host = tempfile::tempdir().unwrap();
        let root = root_with_links(host.path());
        let fd = open_root(root.path()).unwrap();
        let point = host.path().join("point");
        std::fs::create_dir(&point).unwrap();
        // The nemesis: unprivileged, in a user and mount namespace of its own, so it needs no root. It says
        // `ready` once its first mount stands, so the resolves meet a storm already under way.
        let mut nemesis = std::process::Command::new("unshare")
            .args(["-Urm", "sh", "-c"])
            .arg(
                "mount -t tmpfs none \"$0\" && umount \"$0\" && echo ready && \
                 while :; do mount -t tmpfs none \"$0\"; umount \"$0\"; done",
            )
            .arg(&point)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = String::new();
        std::io::BufReader::new(nemesis.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(
            ready.trim(),
            "ready",
            "the nemesis mounts in a namespace of its own (unprivileged user namespaces allowed)"
        );
        // At least 2,000 resolves, and on until one was tried again past an EAGAIN (the nemesis met), at most
        // 50,000: one mount loop crosses a resolve's `..` a few times in 2,000 on fedora.
        let before = retried();
        let mut refused = Vec::new();
        for i in 0..50_000 {
            if i >= 2000 && retried() > before {
                break;
            }
            if let Err(e) = Root::new(fd.as_fd()).open("/up/made/here", Make::Dir) {
                refused.push(e);
            }
        }
        let was_alive = nemesis.try_wait().unwrap().is_none();
        let _ = nemesis.kill();
        let _ = nemesis.wait();
        assert!(was_alive, "the nemesis mounted throughout");
        assert!(
            refused.is_empty(),
            "{} refused: {:?}",
            refused.len(),
            refused.first()
        );
        assert!(
            retried() > before,
            "the nemesis crossed a resolve's `..` and the resolver tried again past it"
        );
    }

    /// Ada's 2026-11-06 rule (AC-T7.1.4, CLAUDE.md §2): the budget is twice the most tries a resolve was measured to
    /// need under a storm, 5 (unit hex-inroot-storm-20261105T130241Z, docs/research/runtime/inroot-eagain.md §3). Do:
    /// a path Linux refuses with EAGAIN 9 times, then answers, and one it refuses 10 times. Expect the first opened,
    /// its 9 retries counted, and the second refused with the EAGAIN, the budget spent.
    #[test]
    fn a_resolve_past_twice_the_measured_need_is_refused() {
        let answer_after = |again: u32| {
            let mut left = again;
            move || match left.checked_sub(1) {
                Some(n) => {
                    left = n;
                    Err(Errno::AGAIN)
                }
                None => rustix::fs::open("/", OFlags::PATH | OFlags::CLOEXEC, Mode::empty()),
            }
        };
        let before = retried();
        assert!(tried(answer_after(9)).is_ok());
        assert_eq!(retried() - before, 9);
        assert_eq!(tried(answer_after(10)).err(), Some(Errno::AGAIN));
    }

    /// CLAUDE.md §3: past its budget the resolver's refusal names the path, the component and the budget, never a
    /// bare EAGAIN. Do: the refusal for an EAGAIN. Expect: all three named.
    #[test]
    fn an_eagain_past_the_budget_is_refused_naming_the_path_and_the_tries() {
        let e = failed("/up/made/here", "up/made", Errno::AGAIN).to_string();
        assert!(
            e.contains("/up/made/here (up/made)")
                && e.contains(&format!("in {IN_ROOT_TRIES} tries"))
                && e.contains("EAGAIN"),
            "{e}"
        );
    }

    /// A relative link climbing out stops at the root, as `..` does at `/`: what's made lands inside.
    #[test]
    fn a_link_climbing_out_stays_at_the_root() {
        let host = tempfile::tempdir().unwrap();
        let root = root_with_links(host.path());
        let fd = open_root(root.path()).unwrap();
        Root::new(fd.as_fd())
            .open("/up/made/here", Make::Dir)
            .unwrap();
        assert!(root.path().join("made/here").is_dir());
        let above = root.path().parent().unwrap().join("made");
        assert!(!above.exists(), "made above the root");
    }

    /// A link to a directory inside the root is followed there, absolute as it is.
    #[test]
    fn a_link_inside_the_root_is_followed_inside() {
        let host = tempfile::tempdir().unwrap();
        let root = root_with_links(host.path());
        let fd = open_root(root.path()).unwrap();
        let in_root = Root::new(fd.as_fd());
        assert!(in_root.open("/inside/hosts", Make::File).unwrap().is_some());
        assert!(root.path().join("real-etc/hosts").is_file());
        let (dir, name) = in_root.parent("/inside/null").unwrap();
        assert_eq!(name, "null");
        rustix::fs::mkdirat(&dir, "by-parent", Mode::RWXU).unwrap();
        assert!(root.path().join("real-etc/by-parent").is_dir());
    }

    /// Missing and asked to make nothing: nothing is made; the root itself is a path too.
    #[test]
    fn nothing_is_made_unless_asked() {
        let root = tempfile::tempdir().unwrap();
        let fd = open_root(root.path()).unwrap();
        let in_root = Root::new(fd.as_fd());
        assert!(in_root.open("/proc/sys", Make::Nothing).unwrap().is_none());
        assert!(!root.path().join("proc").exists());
        assert!(in_root.open("/", Make::Nothing).unwrap().is_some());
        assert!(in_root.parent("/").is_err());
    }

    /// A path written with empty and `.` components, or a trailing slash, resolves to what the
    /// component walk resolves it to: one `openat2` of its normal form, so the file a target names is
    /// the same however its spec spells it (goal 5 plan step 3).
    #[test]
    fn a_path_resolves_the_same_however_it_is_spelled() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("a/b")).unwrap();
        std::fs::write(root.path().join("a/b/f"), "").unwrap();
        let fd = open_root(root.path()).unwrap();
        let in_root = Root::new(fd.as_fd());
        let ino = |dest: &str| {
            let fd = in_root.open(dest, Make::Nothing).unwrap().unwrap();
            rustix::fs::fstat(&fd).unwrap().st_ino
        };
        let want = std::fs::metadata(root.path().join("a/b/f")).unwrap();
        for spelled in ["/a/b/f", "a/b/f", "//a/./b//f", "/a/b/f/", "./a/b/./f"] {
            assert_eq!(
                ino(spelled),
                std::os::unix::fs::MetadataExt::ino(&want),
                "{spelled}"
            );
        }
        assert_eq!(normal("/"), ".");
        assert_eq!(normal("//./"), ".");
        assert_eq!(normal("/a//b/./c/"), "a/b/c");
        assert!(is_root("/") && is_root("//./") && is_root(""));
        assert!(!is_root("/a") && !is_root("/./a/"));
    }
}
