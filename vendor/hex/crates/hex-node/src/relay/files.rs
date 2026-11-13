//! The relay's staging port (docs/design/shards-guest.md §4a): it writes a box's bundle and root, as the
//! host sends them ([`hex_core::stream::files`]), under `<staged>/<box id>/`, and removes that copy when
//! the host deletes the box. Every name is resolved inside the box's staged root (`RESOLVE_IN_ROOT`,
//! openat2(2)): a layer's entry through the layer's own links, inside its root, as containerd's unpack
//! resolves it; any other through no link, so a stream that names its way out through one is refused.
//! Every count is bounded by the core's [`Limits`].

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{self, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use hex_api::{BoxId, Refusal};
use hex_client::guest::Duplex;
use hex_core::stream::{
    self,
    files::{self, Entry, Limits, Meta, Mtime, Op, STORE, Step, Unpack, What},
};
use hex_core::wire::{self, Answer};
use rustix::fs::{
    AtFlags, Dir, FileType, Gid, Mode, OFlags, ResolveFlags, Timespec, Timestamps, Uid,
};
use rustix::io::Errno;

/// How a name under a staged root resolves: inside it, through no symbolic link or magic link and no
/// mount point (openat2(2)).
const RESOLVE: ResolveFlags = ResolveFlags::IN_ROOT
    .union(ResolveFlags::NO_SYMLINKS)
    .union(ResolveFlags::NO_MAGICLINKS)
    .union(ResolveFlags::NO_XDEV);
/// How a layer's entry resolves within its root: through its symbolic links, inside the root, as
/// containerd's `fs.RootPath` does (§4a), through no magic link or mount point.
const WITHIN: ResolveFlags = ResolveFlags::IN_ROOT
    .union(ResolveFlags::NO_MAGICLINKS)
    .union(ResolveFlags::NO_XDEV);
/// How many times an openat2 that the kernel answers `EAGAIN` is tried: it does so when a rename or mount
/// anywhere on the host crossed a `..` during the lookup (openat2(2); fs/namei.c v6.19 :2195-2206), and
/// leaves the retry to the caller. The oci class's resolver's budget (`IN_ROOT_TRIES`,
/// crates/hex-runtime/src/linux/oci/inroot.rs on cp/t7-inroot-eagain): libpathrs's (src/syscalls.rs:864-873),
/// as runc v1.5.2 cites it (`internal/pathrs/retry.go`); measured on fedora under 16 mount and 16 rename
/// loops, no resolve needed more than 5 (unit hex-inroot-storm-20261105T130241Z,
/// docs/research/runtime/inroot-eagain.md). The last `EAGAIN` is returned typed.
const RESOLVE_TRIES: usize = 128;
/// The most links a layer's path follows, as Linux's own lookup (`MAXSYMLINKS`, include/linux/namei.h).
const MAX_LINKS: usize = 40;
/// Linux's bounds on an inode's xattr names together and on one value (`XATTR_LIST_MAX`,
/// `XATTR_SIZE_MAX`, include/uapi/linux/limits.h): what a copied directory's xattrs are read within.
const XATTR_LIST_MAX: usize = files::XATTR_LIST_MAX;
const XATTR_SIZE_MAX: usize = files::XATTR_SIZE_MAX;
/// A staged root's mode once whole: the relay writes it, and the node and a container's mapped users read
/// and search it, which every path through it needs (path_resolution(7), "Permission"): `S_IRWXU` and
/// read and search for group and others (inode(7)).
const ROOT_MODE: u16 = 0o755;
/// A directory or file the relay alone writes while a stage is under way (`S_IRWXU`, `S_IRUSR | S_IWUSR`,
/// inode(7)): its staged mode is set once its contents are whole, and a removal makes a read-only one its
/// own again to empty it.
const OWNER_DIR: u32 = 0o700;
const OWNER_FILE: u32 = 0o600;
/// A directory mkparent makes: `mkdir` 0755 then an explicit chmod past the umask (containerd v2.3.6,
/// `pkg/archive/tar_unix.go:90-97`).
const MKPARENT_MODE: u32 = 0o755;

fn os(op: &'static str) -> impl Fn(Errno) -> Refusal {
    move |e| Refusal::os(op, e)
}

/// Serves one staging connection: reads its op, does it, and answers `Done` or `Refused`. A refusal can come
/// with the host's stream still unread; closing on it then would reset the connection, which over vsock
/// discards the answer in flight, and the host would see a broken pipe, never why (CLAUDE.md §3). So a
/// refused connection is half-closed after its answer and read to its end, within what a stage may still
/// send ([`unread_bound`]), before it closes.
pub fn serve(mut conn: Box<dyn Duplex>, staged: &Path, limits: &Limits) {
    let answer = match take(conn.as_mut(), staged, limits) {
        Ok(()) => Answer::Done,
        Err(why) => Answer::Refused(why),
    };
    let refused = matches!(answer, Answer::Refused(_));
    if let Some(bytes) = wire::answer(0, &answer, wire::MAX_MESSAGE)
        .ok()
        .and_then(|bytes| stream::frame(&bytes, wire::MAX_MESSAGE).ok())
    {
        let _ = conn.write_all(&bytes);
    }
    if refused {
        let _ = conn.shutdown(Shutdown::Write);
        drain(conn.as_mut(), unread_bound(staged, limits));
    }
    let _ = conn.shutdown(Shutdown::Both);
}

fn refused(why: files::Refused) -> Refusal {
    Refusal::Invalid {
        what: "staged files",
        why: why.to_string(),
    }
}

/// Reads the connection to the stream's in-band end and does what it asks. A removal ends at its opening and a
/// stage at its end head ([`files`]), and the relay answers there, not at the connection's end, which a transport
/// need not carry: Firecracker's vsock makes a host's half-close a full shutdown, after which nothing the guest
/// sends reaches the host (`vsock/csm/connection.rs:228-239` and `:307` at upstream 500efc6). Bytes after the end,
/// in what was read, are refused before the end is acted on; past it, the relay reads nothing (AC-T7.4.1).
fn take(conn: &mut dyn Duplex, staged: &Path, limits: &Limits) -> Result<(), Refusal> {
    let mut unpack = Unpack::new(*limits);
    let mut buf = vec![0u8; 64 << 10];
    let mut stage: Option<Stage> = None;
    loop {
        let n = match conn.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return fail(stage, Refusal::os("reading a staging connection", e)),
        };
        let mut input = buf.get(..n).unwrap_or_default();
        while !input.is_empty() {
            let (took, step) = match unpack.take(input) {
                Ok(taken) => taken,
                Err(why) => return fail(stage, refused(why)),
            };
            let data = input.get(..took).unwrap_or_default();
            input = input.get(took..).unwrap_or_default();
            let Some(step) = step else { continue };
            if !input.is_empty() && unpack.finish().is_ok() {
                return fail(stage, refused(files::Refused::Trailing(input.len())));
            }
            let done = match (step, stage.as_mut()) {
                (Step::Open { op, id, layers }, None) => {
                    let id = BoxId::new(&id)?;
                    match op {
                        Op::Remove => remove(staged, &id),
                        Op::Stage => {
                            let (begun, budget) = Stage::begin(staged, id, &layers)?;
                            stage = Some(begun);
                            // The packer learns the stage's budget, and which layers the guest holds,
                            // first; both sides spend against it.
                            unpack.learn(budget.entries, budget.bytes);
                            send_budget(conn, &budget)
                        }
                    }
                }
                (Step::Entry(entry), Some(s)) => s.entry(&entry),
                (Step::Data { done }, Some(s)) => s.data(data, done),
                (Step::End, Some(s)) => s.end(),
                _ => Err(Refusal::Invalid {
                    what: "staged files",
                    why: "a step came out of order".to_owned(),
                }),
            };
            if let Err(why) = done {
                return fail(stage, why);
            }
        }
        if unpack.finish().is_ok() {
            return Ok(());
        }
    }
    if let Err(why) = unpack.finish() {
        return fail(stage, refused(why));
    }
    Ok(())
}

/// What a file system has free and holds, as statvfs(3) reads it: what a stage may spend, learned when
/// it begins (CLAUDE.md §2: a bound the system says, not a number Hex picks).
#[derive(Debug, Clone, Copy)]
struct Free {
    /// The entries a stage may make: free inodes (`f_ffree`), or where the file system keeps no inode
    /// count, its free bytes.
    entries: u64,
    /// Bytes free to an unprivileged writer (`f_bavail` × `f_frsize`; past `u64`, no bound short of it).
    bytes: u64,
    /// The most entries a removal can visit: inodes in use (`f_files` − `f_ffree`), or where the file
    /// system keeps no inode count, its bytes in use.
    used: u64,
    /// The most layers its store can hold, each at least its folder: its inodes (`f_files`), or where it
    /// keeps no count, its bytes.
    capacity: u64,
    /// Its block (`f_frsize`; 1 where it says 0): a file's bytes take whole blocks of it.
    block: u64,
}

/// The fields of statvfs(3) a stage's budget is learned from.
#[derive(Debug, Clone, Copy)]
struct Stat {
    files: u64,
    ffree: u64,
    blocks: u64,
    bfree: u64,
    bavail: u64,
    frsize: u64,
}

fn free(dir: &OwnedFd) -> Result<Free, Refusal> {
    let st = rustix::fs::fstatvfs(dir).map_err(os("reading the staged folder's free space"))?;
    Ok(learned(&Stat {
        files: st.f_files,
        ffree: st.f_ffree,
        blocks: st.f_blocks,
        bfree: st.f_bfree,
        bavail: st.f_bavail,
        frsize: st.f_frsize,
    }))
}

/// What `st` says a stage may spend. A file system that keeps no inode count (btrfs) reports `f_files` 0
/// ("fields that are undefined for a particular filesystem are set to 0", statfs(2)); every entry still
/// takes at least one byte of it, so its bytes bound its entries.
fn learned(st: &Stat) -> Free {
    let bytes = st.bavail.saturating_mul(st.frsize);
    if st.files == 0 {
        return Free {
            entries: bytes,
            bytes,
            used: st.blocks.saturating_sub(st.bfree).saturating_mul(st.frsize),
            capacity: st.blocks.saturating_mul(st.frsize),
            block: st.frsize.max(1),
        };
    }
    Free {
        entries: st.ffree,
        bytes,
        used: st.files.saturating_sub(st.ffree),
        capacity: st.files,
        block: st.frsize.max(1),
    }
}

/// Sends a stage's budget, framed as the relay's answers are.
fn send_budget(conn: &mut dyn Duplex, budget: &files::Budget) -> Result<(), Refusal> {
    let bytes = stream::frame(&files::budget(budget), wire::MAX_MESSAGE)
        .map_err(|e| Refusal::Broken(format!("framing a stage's budget: {e}")))?;
    conn.write_all(&bytes)
        .map_err(|e| Refusal::os("sending a stage's budget", e))
}

/// The most a stage's stream can still hold, learned as its budget is: the bytes the staged folder's file
/// system has free (file and xattr bytes), and for each entry it has inodes free for, a head, a path and a
/// link target at the path bound, and an owner and times. Past it the stream is no stage's, and the relay
/// closes on it.
fn unread_bound(staged: &Path, limits: &Limits) -> u64 {
    let Ok(dir) = rustix::fs::open(
        staged,
        OFlags::DIRECTORY | OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
    ) else {
        return 0;
    };
    let Ok(free) = free(&dir) else {
        return 0;
    };
    let entry = files::HEAD_BYTES
        .saturating_add(limits.path.saturating_mul(2))
        .saturating_add(files::META_BYTES);
    free.bytes
        .saturating_add(
            free.entries
                .saturating_mul(u64::try_from(entry).unwrap_or(u64::MAX)),
        )
        .saturating_add(u64::try_from(files::HEAD_BYTES).unwrap_or(u64::MAX))
}

/// Reads `conn` to its end, at most `bound` bytes, discarding them: what the host sent past a refusal.
fn drain(conn: &mut dyn Duplex, bound: u64) {
    let mut buf = vec![0u8; wire::MAX_MESSAGE];
    let mut read: u64 = 0;
    while read <= bound {
        match conn.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => read = read.saturating_add(u64::try_from(n).unwrap_or(u64::MAX)),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// Removes a stage's partial copy, then returns why it failed.
fn fail<T>(stage: Option<Stage>, why: Refusal) -> Result<T, Refusal> {
    if let Some(stage) = stage {
        let _ = stage.abandon();
    }
    Err(why)
}

/// A box's record of its root's layers, lowest first, a digest a line, in its staged folder: what makes a
/// layer referenced. Written when its stage opens, so a layer it names is never evicted under it.
const RECORD: &str = ".layers";
/// A layer being staged: `.partial-<relay pid>-<n>-<digest>`, renamed to `<digest>` only once whole, so a
/// stage cut or crashed mid-layer leaves no layer that counts as present.
const PARTIAL: &str = ".partial-";
/// Beside a layer `<digest>`, its `<digest>.size`: the entries and bytes it holds, what its eviction frees.
const SIZE: &str = ".size";
/// A layer a stage out of room took out of the store, being removed: `.gone-<relay pid>-<n>`.
const GONE: &str = ".gone-";
/// In the store, the room it leaves free for a stage: the most entries and bytes one has spent (`.need`),
/// and its lock (`.lock`).
const NEED: &str = ".need";
const LOCK: &str = ".lock";
/// A box's folder of its root's layers, `<i>` each: a link to the store's layer or a layer of its own.
const BOX_LAYERS: &str = "layers";

/// What a stage spends, or must leave free: entries and bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Spend {
    entries: u64,
    bytes: u64,
}

impl Spend {
    fn plus(self, other: Spend) -> Spend {
        Spend {
            entries: self.entries.saturating_add(other.entries),
            bytes: self.bytes.saturating_add(other.bytes),
        }
    }
}

/// A stage under way: its root, the layer being applied, the file being written, and the directories whose
/// modes are set last.
struct Stage {
    staged: OwnedFd,
    id: BoxId,
    root: OwnedFd,
    /// The root's own path in the guest.
    root_path: String,
    /// The layer store; the box's `layers` folder, each `<i>` a link to a layer the store holds or a layer
    /// staged into the box; the layers this stage names, by digest (none for one the host knows no digest
    /// of), lowest first, and which it is to stage.
    store: OwnedFd,
    layers: Option<OwnedFd>,
    wanted: Vec<Option<String>>,
    absent: VecDeque<usize>,
    /// What the staged folder's file system had free when the stage began, and what the store leaves free
    /// for a stage: the most one has taken of it (learned, rising only). A layer is admitted to the store
    /// only while what's free stays at least that ([`admits`]); past it, it and every later layer of the
    /// stage go into the box (what's free only falls). Nothing cached is taken for a stage that goes into the
    /// box: only one that runs out of room takes any ([`Stage::room`]).
    free: Free,
    need: Spend,
    /// Whether this stage may admit any layer: it began with the room a stage needs. One that began without
    /// it admits none, so its packer, told so in the budget, need not verify what it sends.
    admitting: bool,
    /// The file system's free room as last read, and what this stage has charged since: free room is read
    /// again only once that nears the margin over `need` ([`Stage::admits_now`]), not at every entry.
    last: Free,
    since: Spend,
    /// Counted for HEX-GUEST-14's check that admission costs a bounded few: the file system's free room
    /// read (fstatvfs) during the stage, and the store's lock taken.
    reads: u64,
    locks: u64,
    /// The file being written, with what's applied once its last byte is (§4a's order).
    file: Option<(std::fs::File, String, u16, Option<Meta>)>,
    /// Each directory's mode and times, by where it is (in the layer under way, or under the box's root)
    /// and its resolved path there, the latest header's: set when its layer ends (or the stage, outside a
    /// layer), deepest first, after everything in it (`tar.go:330-337`).
    dirs: BTreeMap<(Base, String), (u16, Option<Timestamps>)>,
    /// The layer being applied, if any.
    layer: Option<Layer>,
    /// Entries made so far: what an abandoned stage's removal visits.
    entries: usize,
    /// What removals during the stage (a replaced entry, a whiteout) may visit.
    removals: Budget,
    /// The xattr that makes a layer's directory opaque to the overlay the guest's node mounts:
    /// `trusted.overlay.opaque` where the relay runs as root and the node mounts as root, else
    /// `user.overlay.opaque`, as a rootless node mounts with `userxattr` (Documentation/filesystems/
    /// overlayfs.rst, "Whiteouts and opaque directories"; `hex_core::bundle`'s `overlay`).
    opaque_xattr: &'static str,
    /// Layers this stage committed, counted for none to commit twice.
    seq: u64,
    /// Reads the staged folder's file system's free room after the stage began ([`free`]): what admission
    /// ([`Stage::admits_now`]) and what the stage took ([`Stage::end`]) are read from. A test pins it, so
    /// what other writers of the same file system do meanwhile decides nothing of its own.
    read_free: fn(&OwnedFd) -> Result<Free, Refusal>,
}

/// Where a directory whose mode and times are set last is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Base {
    Root,
    Layer,
}

/// A layer under way (§4a): its folder, which its entries' names resolve within as containerd's overlay
/// applier resolves them in a layer's own upper directory, the layers below it (nearest first), whose
/// directories a parent this layer makes copies its attributes from, and, admitted to the store, where it's
/// committed once whole.
struct Layer {
    /// Its root's path in the stream, which its entries' paths begin with.
    path: String,
    fd: OwnedFd,
    /// Its folder's own path in the guest.
    base: String,
    lowers: Vec<OwnedFd>,
    /// Its place in the stage's list, and whether it's an overlay's layer (whiteouts and opaque markers as
    /// overlayfs's own) rather than a root of one copy applied in place.
    index: usize,
    overlay: bool,
    /// What it holds so far: what its eviction frees.
    held: Spend,
    /// Admitted to the store: its digest and its partial name.
    store: Option<Pending>,
}

struct Pending {
    digest: String,
    partial: String,
}

/// A count of entries a removal may still visit, refused by type past its bound (CLAUDE.md §2).
struct Budget {
    left: usize,
    bound: usize,
}

impl Budget {
    fn new(bound: usize) -> Self {
        Self { left: bound, bound }
    }

    fn spend(&mut self) -> Result<(), Refusal> {
        self.left = self.left.checked_sub(1).ok_or(Refusal::Exhausted {
            what: "entries one removal of a staged copy visits",
            bound: self.bound,
        })?;
        Ok(())
    }
}

/// A staged entry's times, as utimensat(2) takes them: its atime and mtime (§4a).
fn times(meta: &Meta) -> Timestamps {
    let at = |t: Mtime| Timespec {
        tv_sec: t.secs,
        tv_nsec: i64::from(t.nanos),
    };
    Timestamps {
        last_access: at(meta.atime),
        last_modification: at(meta.mtime),
    }
}

fn owner(meta: &Meta) -> (Option<Uid>, Option<Gid>) {
    (Some(Uid::from_raw(meta.uid)), Some(Gid::from_raw(meta.gid)))
}

/// An extended attribute the guest refused to set, by name: the stage is refused rather than staged
/// without it (CLAUDE.md §3).
fn refused_xattr(path: &str, name: &str, e: Errno) -> Refusal {
    Refusal::Invalid {
        what: "staged files",
        why: format!("{path:?}: the guest refused extended attribute {name}: {e}"),
    }
}

fn invalid(why: String) -> Refusal {
    Refusal::Invalid {
        what: "staged files",
        why,
    }
}

/// Applies a file's or directory's owner and xattrs, then (for a file) its mode and times, on its open
/// descriptor, in §4a's order: owner, mode, xattrs, times. A directory's mode and times wait for its
/// layer's end (`mode_and_time` false).
fn apply(
    fd: &OwnedFd,
    path: &str,
    mode: u16,
    meta: Option<&Meta>,
    mode_and_time: bool,
) -> Result<(), Refusal> {
    if let Some(meta) = meta {
        let (uid, gid) = owner(meta);
        rustix::fs::fchown(fd, uid, gid).map_err(os("setting a staged entry's owner"))?;
    }
    if mode_and_time {
        rustix::fs::fchmod(fd, Mode::from_raw_mode(mode.into()))
            .map_err(os("setting a staged entry's mode"))?;
    }
    if let Some(meta) = meta {
        for x in &meta.xattrs {
            rustix::fs::fsetxattr(
                fd,
                x.name.as_str(),
                &x.value,
                rustix::fs::XattrFlags::empty(),
            )
            .map_err(|e| refused_xattr(path, &x.name, e))?;
        }
        if mode_and_time {
            rustix::fs::futimens(fd, &times(meta)).map_err(os("setting a staged entry's times"))?;
        }
    }
    Ok(())
}

/// `rel`'s parent and last component: `("", rel)` for a name at the top.
fn split(rel: &str) -> (&str, &str) {
    rel.rsplit_once('/').unwrap_or(("", rel))
}

/// `leaf` under the resolved directory `dir`, `""` being the root.
fn join(dir: &str, leaf: &str) -> String {
    if dir.is_empty() {
        leaf.to_owned()
    } else {
        format!("{dir}/{leaf}")
    }
}

/// What a removal under `dir` may visit: no more entries than its file system holds (statvfs(3)).
fn fs_budget(dir: &OwnedFd) -> Budget {
    Budget::new(free(dir).map_or(0, |f| usize::try_from(f.used).unwrap_or(usize::MAX)))
}

/// Whether `name` is a layer's name in the store: 64 lowercase hex digits.
fn is_layer_name(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Now, as utimensat(2) takes it: a layer's last use, by which the least recently used is evicted.
fn now() -> Timestamps {
    let now = Timespec {
        tv_sec: 0,
        tv_nsec: rustix::fs::UTIME_NOW,
    };
    Timestamps {
        last_access: now,
        last_modification: now,
    }
}

thread_local! {
    /// This thread's directory listings ([`names`]), counted for HEX-GUEST-14's check that a stage's
    /// opening lists the store once.
    static LISTINGS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The names in the directory `dir`, read whole: a store or a staged folder, bounded by the file system's
/// own entries.
fn names(dir: &OwnedFd) -> Result<Vec<String>, Refusal> {
    LISTINGS.with(|n| n.set(n.get().saturating_add(1)));
    let mut out = Vec::new();
    for entry in Dir::read_from(dir).map_err(os("listing a staged folder"))? {
        let entry = entry.map_err(os("listing a staged folder"))?;
        let name = entry.file_name();
        if name == c"." || name == c".." {
            continue;
        }
        if let Ok(name) = name.to_str() {
            out.push(name.to_owned());
        }
    }
    Ok(out)
}

/// Every layer some box's record names: the layers no eviction may take.
fn referenced(staged: &OwnedFd) -> Result<BTreeSet<String>, Refusal> {
    let mut held = BTreeSet::new();
    for name in names(staged)? {
        if name.starts_with('.') {
            continue;
        }
        let Ok(fd) = rustix::fs::openat(
            staged,
            format!("{name}/{RECORD}").as_str(),
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) else {
            continue;
        };
        let mut text = String::new();
        io::Read::read_to_string(&mut std::fs::File::from(fd), &mut text)
            .map_err(|e| Refusal::os("reading a box's layer record", e))?;
        held.extend(text.lines().filter(|l| is_layer_name(l)).map(str::to_owned));
    }
    Ok(held)
}

/// Writes `text` as `name` under `dir` whole: to a temporary name, then renamed over it, so a reader never
/// sees part of it.
fn write_whole(dir: &OwnedFd, name: &str, text: &str) -> Result<(), Refusal> {
    let tmp = format!(".tmp-{}-{name}", std::process::id());
    let fd = rustix::fs::openat(
        dir,
        tmp.as_str(),
        OFlags::CREATE | OFlags::TRUNC | OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(OWNER_FILE),
    )
    .map_err(os("writing a staged record"))?;
    std::fs::File::from(fd)
        .write_all(text.as_bytes())
        .map_err(|e| Refusal::os("writing a staged record", e))?;
    rustix::fs::renameat(dir, tmp.as_str(), dir, name).map_err(os("writing a staged record"))
}

impl Stage {
    /// Makes `<staged>/<id>`, which must not exist: one there is another box's, or a copy the host
    /// hasn't removed, and staging over it would change what that box runs. Records the layers `layers`
    /// names in it and links each the store holds from its `layers` folder, under the store's lock, so no
    /// eviction takes one it counted as held; says which the store holds and what the stage may spend.
    fn begin(
        staged: &Path,
        id: BoxId,
        layers: &[Option<files::Digest>],
    ) -> Result<(Self, files::Budget), Refusal> {
        let staged = rustix::fs::open(
            staged,
            OFlags::DIRECTORY | OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(os("opening the staged folder"))?;
        match rustix::fs::mkdirat(&staged, id.as_str(), Mode::from_raw_mode(OWNER_DIR)) {
            Ok(()) => {}
            Err(Errno::EXIST) => return Err(Refusal::Exists(id)),
            Err(e) => return Err(Refusal::os("making a box's staged root", e)),
        }
        let root = rustix::fs::openat(
            &staged,
            id.as_str(),
            OFlags::DIRECTORY | OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(os("opening a box's staged root"))?;
        let root_path = fd_path(&root)?;
        let store = open_store(&staged)?;
        let wanted: Vec<Option<String>> = layers
            .iter()
            .map(|d| d.as_ref().map(files::digest_hex))
            .collect();
        let layers_dir = if wanted.is_empty() {
            None
        } else {
            rustix::fs::mkdirat(&root, BOX_LAYERS, Mode::from_raw_mode(OWNER_DIR))
                .map_err(os("making a box's layers folder"))?;
            Some(
                rustix::fs::openat(
                    &root,
                    BOX_LAYERS,
                    OFlags::DIRECTORY | OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(os("opening a box's layers folder"))?,
            )
        };
        let mut present = Vec::with_capacity(wanted.len());
        let mut absent = VecDeque::new();
        let (free, need, admitting, spare) = {
            let _lock = lock(&store)?;
            // One listing of the store: what another relay process left part way (a layer, an evicted one
            // going) is removed; the rest says which layers it holds and what the last resort can take.
            let me = std::process::id().to_string();
            let mut layers_held = BTreeSet::new();
            let mut going = Spend::default();
            for name in names(&store)? {
                if name.starts_with(PARTIAL) || name.starts_with(GONE) {
                    let mine = [PARTIAL, GONE].iter().any(|p| {
                        name.strip_prefix(p)
                            .is_some_and(|r| r.starts_with(&format!("{me}-")))
                    });
                    if !mine {
                        if name.ends_with(SIZE) {
                            let _ = rustix::fs::unlinkat(&store, name.as_str(), AtFlags::empty());
                        } else {
                            let mut budget = fs_budget(&store);
                            let _ = remove_tree(&store, &name, &mut budget);
                        }
                    } else if name.starts_with(GONE) && name.ends_with(SIZE) {
                        // A layer being taken out, its room soon free: a stage out of room finishes it.
                        going = going.plus(read_spend(&store, &name));
                    }
                } else if is_layer_name(&name) {
                    layers_held.insert(name);
                }
            }
            let need = read_spend(&store, NEED);
            let free = free(&staged)?;
            let admitting = admits(free, Spend::default(), need);
            for digest in &wanted {
                present.push(digest.as_ref().is_some_and(|d| layers_held.contains(d)));
            }
            // The record before any link, so no eviction takes a layer this stage names while it stages;
            // a stage that admits nothing and holds nothing names no layer the store holds, and writes none.
            let named: Vec<&str> = wanted.iter().flatten().map(String::as_str).collect();
            if !named.is_empty() && (admitting || present.iter().any(|p| *p)) {
                write_whole(&root, RECORD, &(named.join("\n") + "\n"))?;
            }
            for (i, digest) in wanted.iter().enumerate() {
                match (
                    present.get(i).copied().unwrap_or(false),
                    digest,
                    layers_dir.as_ref(),
                ) {
                    (true, Some(d), Some(dir)) => {
                        // Its last use, for the least recently used to go first, and the box's link to it.
                        let _ = rustix::fs::utimensat(
                            &store,
                            d.as_str(),
                            &now(),
                            AtFlags::SYMLINK_NOFOLLOW,
                        );
                        link_layer(dir, i, d)?;
                    }
                    _ => absent.push_back(i),
                }
            }
            // What the last resort can take for this stage: every layer no box names (this stage's own record
            // is written if it names any the store holds), as their size files say, and what's going.
            let held = referenced(&staged)?;
            let mut spare = going;
            for name in &layers_held {
                if !held.contains(name) {
                    spare = spare.plus(read_spend(&store, &format!("{name}{SIZE}")));
                }
            }
            (free, need, admitting, spare)
        };
        let opaque_xattr = if rustix::process::geteuid().is_root() {
            "trusted.overlay.opaque"
        } else {
            "user.overlay.opaque"
        };
        Ok((
            Self {
                staged,
                id,
                root,
                root_path,
                store,
                layers: layers_dir,
                wanted,
                absent,
                free,
                need,
                admitting,
                last: free,
                since: Spend::default(),
                reads: 0,
                locks: 1,
                file: None,
                dirs: BTreeMap::new(),
                layer: None,
                entries: 0,
                // A removal visits no more entries than the file system holds.
                removals: Budget::new(usize::try_from(free.used).unwrap_or(usize::MAX)),
                opaque_xattr,
                seq: 0,
                read_free: self::free,
            },
            // What the stage may spend: what's free, and what the last resort can take ([`Stage::room`]),
            // so the packer never refuses a stage whole-root staging would have staged.
            files::Budget {
                entries: free.entries.saturating_add(spare.entries),
                bytes: free.bytes.saturating_add(spare.bytes),
                present,
                capacity: free.capacity,
                admitting,
            },
        ))
    }

    /// Where the directory `fd` is under the layer under way, or else under the box's root, by the
    /// kernel's own resolution of it.
    fn resolved(&self, fd: &OwnedFd) -> Result<String, Refusal> {
        let path = fd_path(fd)?;
        let base = self
            .layer
            .as_ref()
            .map_or(self.root_path.as_str(), |l| l.base.as_str());
        let rest = path.strip_prefix(base).ok_or_else(|| {
            Refusal::Broken(format!(
                "a staged directory, {path:?}, resolved outside its root"
            ))
        })?;
        Ok(rest.trim_start_matches('/').to_owned())
    }

    /// The directory that holds `path` and `path`'s last component: as the layer's overlay unpack finds it
    /// when `path` is a layer's entry ([`Stage::walk`]; its parents made when `make`, as an entry's are and
    /// a hard link's target's aren't), else inside the stage's root through no link.
    fn parent<'p>(&mut self, path: &'p str, make: bool) -> Result<(OwnedFd, &'p str), Refusal> {
        if let Some(rel) = self.in_layer(path).map(|(_, rel)| rel) {
            let (parent, leaf) = split(rel);
            return Ok((self.walk(parent, make)?, leaf));
        }
        let (parent, leaf) = path.rsplit_once('/').unwrap_or((".", path));
        Ok((open_in(&self.root, parent)?, leaf))
    }

    /// The directory `rel` within the layer's folder, as containerd's overlay applier finds it: it unpacks
    /// a layer into its own upper directory (`core/diff/apply/apply_linux.go:36-55` at v2.3.6), so
    /// `fs.RootPath` follows only links this layer made, inside the layer; a name this layer didn't make
    /// isn't there, and mkparent makes a directory there (`pkg/archive/tar.go:448-503`), copying the owner,
    /// mode, times and xattrs but `trusted.*` of the nearest layer below whose view of the name is a
    /// directory (`WithParents`, `:482-500`; `copyDirInfo`, `copyUpXAttrs`, `tar_unix.go:143-203`), else
    /// 0755. A name missing (when not `make`) is refused. Flat: one step per component, at most
    /// [`MAX_LINKS`] links followed, each component opened `O_NOFOLLOW`.
    fn walk(&mut self, rel: &str, make: bool) -> Result<OwnedFd, Refusal> {
        let layer = self
            .layer
            .as_ref()
            .ok_or_else(|| invalid(format!("{rel:?} is in no layer")))?;
        let top = layer
            .fd
            .try_clone()
            .map_err(|e| Refusal::os("opening a layer's root", e))?;
        let mut fd = top
            .try_clone()
            .map_err(|e| Refusal::os("opening a layer's root", e))?;
        // The components below the layer's root reached so far, and those still to walk.
        let mut at: Vec<String> = Vec::new();
        let mut todo: VecDeque<String> = rel.split('/').map(str::to_owned).collect();
        let mut links: usize = 0;
        while let Some(part) = todo.pop_front() {
            if part.is_empty() || part == "." {
                continue;
            }
            if part == ".." {
                // Inside the root, as RESOLVE_IN_ROOT holds it: `..` at the root stays there.
                at.pop();
                fd = if at.is_empty() {
                    top.try_clone()
                        .map_err(|e| Refusal::os("opening a layer's root", e))?
                } else {
                    open_in(&top, &at.join("/"))?
                };
                continue;
            }
            let below = if at.is_empty() {
                part.clone()
            } else {
                format!("{}/{part}", at.join("/"))
            };
            match rustix::fs::statat(&fd, part.as_str(), AtFlags::SYMLINK_NOFOLLOW)
                .map(|st| FileType::from_raw_mode(st.st_mode))
            {
                Ok(FileType::Directory) => {}
                Ok(FileType::Symlink) => {
                    links = links.saturating_add(1);
                    if links > MAX_LINKS {
                        return Err(invalid(format!(
                            "{rel:?} follows more than {MAX_LINKS} links"
                        )));
                    }
                    let target = rustix::fs::readlinkat(&fd, part.as_str(), Vec::new())
                        .map_err(os("reading a staged link"))?
                        .into_string()
                        .map_err(|_| invalid(format!("{below:?}'s target isn't UTF-8")))?;
                    if target.starts_with('/') {
                        at.clear();
                        fd = top
                            .try_clone()
                            .map_err(|e| Refusal::os("opening a layer's root", e))?;
                    }
                    for (i, step) in target.split('/').enumerate() {
                        todo.insert(i, step.to_owned());
                    }
                    continue;
                }
                Ok(_) => {
                    return Err(invalid(format!(
                        "{rel:?} leads through {below:?}, which this layer made a file"
                    )));
                }
                Err(Errno::NOENT) if !make => {
                    return Err(invalid(format!(
                        "{rel:?} isn't staged: {below:?} is missing"
                    )));
                }
                Err(Errno::NOENT) => {
                    self.room(|| {
                        rustix::fs::mkdirat(&fd, part.as_str(), Mode::from_raw_mode(MKPARENT_MODE))
                    })
                    .map_err(|e| invalid(format!("making {below:?}, a parent: {e}")))?;
                    let dir = rustix::fs::openat(
                        &fd,
                        part.as_str(),
                        OFlags::DIRECTORY | OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .map_err(os("opening a parent directory staging made"))?;
                    match self.below(&below) {
                        Some(from) => copy_dir_info(&from, &dir)?,
                        None => rustix::fs::fchmod(&dir, Mode::from_raw_mode(MKPARENT_MODE))
                            .map_err(os("setting a made parent's mode"))?,
                    }
                }
                Err(e) => {
                    return Err(Refusal::Os {
                        op: "walking a staged path",
                        why: format!("{below:?}: {e}"),
                    });
                }
            }
            fd = rustix::fs::openat(
                &fd,
                part.as_str(),
                OFlags::DIRECTORY | OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(os("opening a staged directory"))?;
            at.push(part);
        }
        Ok(fd)
    }

    /// The nearest layer below's view of `rel` when it's a directory, whose attributes a parent mkparent
    /// makes copies; none when no layer below holds it, or the nearest holds no directory there
    /// ("Replaced, do not copy attributes", `tar.go:490-492`).
    fn below(&self, rel: &str) -> Option<OwnedFd> {
        let layer = self.layer.as_ref()?;
        for lower in &layer.lowers {
            match within(lower, rel) {
                Ok(dir) => return Some(dir),
                Err(Errno::NOENT) => {}
                Err(_) => return None,
            }
        }
        None
    }

    /// Whether `rel` is in a layer below this one: a hard link to it is an earlier layer's.
    fn in_lower(&self, rel: &str) -> bool {
        let (parent, leaf) = split(rel);
        self.layer.as_ref().is_some_and(|layer| {
            layer.lowers.iter().any(|lower| {
                within(lower, parent).is_ok_and(|dir| {
                    rustix::fs::statat(&dir, leaf, AtFlags::SYMLINK_NOFOLLOW).is_ok()
                })
            })
        })
    }

    /// Whether the store may take `entry` ([`admits`]), from the free room last read less what this stage has
    /// charged since, read again only once those charges reach half the margin the last read left over
    /// `need`, or that estimate finds no room: as exact as reading at every entry while what's charged is
    /// well inside the margin, and a few reads a stage, not one an entry (alpine's 515 cost +1.1 ms, d51a4da).
    /// Another stage taking room meanwhile is the last resort's ([`Stage::room`]).
    fn admits_now(&mut self, entry: Spend) -> Result<bool, Refusal> {
        let charged = self.since.plus(entry);
        // The margin the last read left over `need`; none (it held less than `need`) is a read.
        let margin = self
            .last
            .entries
            .checked_sub(self.need.entries)
            .zip(self.last.bytes.checked_sub(self.need.bytes));
        if let Some((entries, bytes)) = margin
            && charged.entries <= entries / 2
            && charged.bytes <= bytes / 2
        {
            self.since = charged;
            return Ok(true);
        }
        self.last = (self.read_free)(&self.staged)?;
        self.reads = self.reads.saturating_add(1);
        self.since = entry;
        Ok(admits(self.last, entry, self.need))
    }

    /// Runs `op`, a write of this stage's, and when the file system has no room for it (`ENOSPC`) takes the
    /// least recently used layer no box names out of the store and tries again, until it fits or none is
    /// left, when `ENOSPC` is the refusal: a last resort, so a create never fails where whole-root staging
    /// would have staged it (the lead's rule, 2026-11-06). Admission leaves the room a stage needs, so this
    /// runs only for a stage larger than any yet, once: that stage raises the room learned. Bounded by the
    /// store's layers: each retry follows one taken.
    fn room<T>(&self, mut op: impl FnMut() -> Result<T, Errno>) -> Result<T, Errno> {
        loop {
            match op() {
                Err(Errno::NOSPC) => match evict_one(&self.staged, &self.store) {
                    Ok(Some(_)) => {}
                    Ok(None) => match finish_gone(&self.store) {
                        Ok(true) => {}
                        Ok(false) | Err(_) => return Err(Errno::NOSPC),
                    },
                    Err(_) => return Err(Errno::NOSPC),
                },
                done => return done,
            }
        }
    }

    /// The layer under way and `path` within its root, when `path` is under it.
    fn in_layer<'p>(&self, path: &'p str) -> Option<(&Layer, &'p str)> {
        let layer = self.layer.as_ref()?;
        let rest = path.strip_prefix(layer.path.as_str())?;
        if rest.is_empty() {
            return Some((layer, ""));
        }
        Some((layer, rest.strip_prefix('/')?))
    }

    /// Counts an entry of `bytes` (a file's and its xattrs'), in whole blocks, into what the layer under way
    /// holds. A layer in the store whose entry would leave less free than a stage needs ([`admits`], from
    /// the file system's free room now) is moved into the box, where the rest of it and every later layer of
    /// the stage go: the store admits a layer only into room already free.
    fn count(&mut self, bytes: u64) -> Result<(), Refusal> {
        let entry = charge(bytes, self.free.block);
        let Some(layer) = self.layer.as_mut() else {
            return Ok(());
        };
        layer.held = layer.held.plus(entry);
        if layer.store.is_none() {
            // Into the box: charged, with no read and no admission to decide.
            self.since = self.since.plus(entry);
            return Ok(());
        }
        if self.admits_now(entry)? {
            return Ok(());
        }
        let Some(layer) = self.layer.as_mut() else {
            return Ok(());
        };
        let (Some(pending), Some(dir)) = (layer.store.take(), self.layers.as_ref()) else {
            return Ok(());
        };
        rustix::fs::renameat(
            &self.store,
            pending.partial.as_str(),
            dir,
            layer.index.to_string().as_str(),
        )
        .map_err(os("moving a layer the store has no room for into its box"))?;
        layer.base = fd_path(&layer.fd)?;
        Ok(())
    }

    fn entry(&mut self, entry: &Entry) -> Result<(), Refusal> {
        if self.file.is_some() {
            return Err(invalid("an entry came in the middle of a file".to_owned()));
        }
        self.entries = self.entries.saturating_add(1);
        match &entry.what {
            What::Layer => return self.layer(&entry.path),
            What::Opaque => return self.opaque(&entry.path),
            _ => {}
        }
        let layered = self.in_layer(&entry.path).is_some();
        let base = if layered { Base::Layer } else { Base::Root };
        let (parent, leaf) = self.parent(&entry.path, true)?;
        let at = join(&self.resolved(&parent)?, leaf);
        self.count(
            match entry.what {
                What::File { size } => size,
                _ => 0,
            }
            .saturating_add(entry.meta.as_ref().map_or(0, files::xattr_bytes)),
        )?;
        let overlay = self.layer.as_ref().is_some_and(|l| l.overlay);
        if matches!(entry.what, What::Whiteout) {
            if !layered {
                return Err(invalid(format!("whiteout {:?} is in no layer", entry.path)));
            }
            if !overlay {
                // A root applied in place, with nothing below it: the whiteout removes what this layer
                // made, as containerd's naive applier's `os.RemoveAll` (`tar.go:218-222`).
                return self.remove(&parent, leaf, &at);
            }
            // In an overlay's layer, a whiteout is overlayfs's own: a character device 0/0 hiding the name below
            // (containerd's `OverlayConvertWhiteout`, pkg/archive/tar_opts_linux.go; Documentation/
            // filesystems/overlayfs.rst, "Whiteouts and opaque directories"); one over a name this layer
            // made is refused, as its mknod fails there (EEXIST).
            return self
                .room(|| {
                    rustix::fs::mknodat(
                        &parent,
                        leaf,
                        FileType::CharacterDevice,
                        Mode::empty(),
                        rustix::fs::makedev(0, 0),
                    )
                })
                .map_err(|e| invalid(format!("whiteout {:?}: {e}", entry.path)));
        }
        if layered {
            // A layer's entry replaces what this layer put at its path, but a directory onto a directory
            // merges and takes this header's owner, mode, xattrs and times (`tar.go:304-311`).
            match rustix::fs::statat(&parent, leaf, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(st)
                    if matches!(entry.what, What::Dir)
                        && FileType::from_raw_mode(st.st_mode) == FileType::Directory => {}
                Ok(_) => self.remove(&parent, leaf, &at)?,
                Err(Errno::NOENT) => {}
                Err(e) => {
                    return Err(Refusal::Os {
                        op: "staging an entry",
                        why: format!("{:?}: {e}", entry.path),
                    });
                }
            }
        }
        let exists = |e: Errno| match e {
            Errno::EXIST => invalid(format!("{:?} came twice", entry.path)),
            e => Refusal::Os {
                op: "staging an entry",
                why: format!("{:?}: {e}", entry.path),
            },
        };
        let meta = entry.meta.as_ref();
        match &entry.what {
            What::Dir => {
                // Writable by the relay until its layer ends, whatever its mode: its entries come after it.
                match self
                    .room(|| rustix::fs::mkdirat(&parent, leaf, Mode::from_raw_mode(OWNER_DIR)))
                {
                    Err(Errno::EXIST) if layered => {}
                    done => done.map_err(exists)?,
                }
                if meta.is_some() {
                    let dir = rustix::fs::openat(
                        &parent,
                        leaf,
                        OFlags::DIRECTORY | OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .map_err(os("opening a staged directory"))?;
                    apply(&dir, &entry.path, entry.mode, meta, false)?;
                }
                self.dirs
                    .insert((base, at.clone()), (entry.mode, meta.map(times)));
            }
            What::File { size } => {
                let fd = self
                    .room(|| {
                        rustix::fs::openat(
                            &parent,
                            leaf,
                            OFlags::CREATE
                                | OFlags::EXCL
                                | OFlags::WRONLY
                                | OFlags::NOFOLLOW
                                | OFlags::CLOEXEC,
                            Mode::from_raw_mode(OWNER_FILE),
                        )
                    })
                    .map_err(exists)?;
                if *size > 0 {
                    self.file = Some((
                        std::fs::File::from(fd),
                        entry.path.clone(),
                        entry.mode,
                        entry.meta.clone(),
                    ));
                } else {
                    apply(&fd, &entry.path, entry.mode, meta, true)?;
                }
            }
            What::Symlink { target } => {
                self.room(|| rustix::fs::symlinkat(target.as_str(), &parent, leaf))
                    .map_err(exists)?;
                if let Some(meta) = meta {
                    let (uid, gid) = owner(meta);
                    rustix::fs::chownat(&parent, leaf, uid, gid, AtFlags::SYMLINK_NOFOLLOW)
                        .map_err(os("setting a staged link's owner"))?;
                    // A link has no descriptor to set an xattr through: its path through the parent's
                    // descriptor, which the relay alone writes under.
                    let at = format!("/proc/self/fd/{}/{leaf}", parent.as_raw_fd());
                    for x in &meta.xattrs {
                        rustix::fs::lsetxattr(
                            at.as_str(),
                            x.name.as_str(),
                            &x.value,
                            rustix::fs::XattrFlags::empty(),
                        )
                        .map_err(|e| refused_xattr(&entry.path, &x.name, e))?;
                    }
                    rustix::fs::utimensat(&parent, leaf, &times(meta), AtFlags::SYMLINK_NOFOLLOW)
                        .map_err(os("setting a staged link's times"))?;
                }
            }
            What::Hardlink { target } => {
                // The target is resolved as an entry's path is (within this layer's folder, as
                // containerd's `hardlinkRootPath` does in the upper directory, `tar.go:795-807`); linkat(2)
                // follows no link at its last component, as containerd's `link` (os.Link) doesn't, so a
                // link names the link itself. A target in an earlier layer, or none, is refused (ENOENT
                // in this layer; ctr, lab:hardlink), and a directory, which link(2) refuses, by name.
                let rel = self
                    .in_layer(target)
                    .map(|(_, rel)| rel.to_owned())
                    .unwrap_or_default();
                let earlier = || {
                    invalid(format!(
                        "hard link {:?} names {target:?}, a file an earlier layer made, which \
                         containerd's overlay unpack can't link (ENOENT in this layer)",
                        entry.path
                    ))
                };
                let (from, from_leaf) = match self.parent(target, false) {
                    Ok(found) => found,
                    Err(_) if layered && self.in_lower(&rel) => return Err(earlier()),
                    Err(why) => return Err(why),
                };
                let st = match rustix::fs::statat(&from, from_leaf, AtFlags::SYMLINK_NOFOLLOW) {
                    Ok(st) => st,
                    Err(_) if layered && self.in_lower(&rel) => return Err(earlier()),
                    Err(e) => {
                        return Err(invalid(format!(
                            "hard link {:?} names {target:?}, which isn't staged: {e}",
                            entry.path
                        )));
                    }
                };
                if !matches!(
                    FileType::from_raw_mode(st.st_mode),
                    FileType::RegularFile | FileType::Symlink
                ) {
                    return Err(invalid(format!(
                        "hard link {:?} names {target:?}, which is neither a file nor a link",
                        entry.path
                    )));
                }
                self.room(|| rustix::fs::linkat(&from, from_leaf, &parent, leaf, AtFlags::empty()))
                    .map_err(exists)?;
            }
            What::Layer | What::Whiteout | What::Opaque => {}
        }
        Ok(())
    }

    /// A layer begins at `path`. In a stage naming its layers, it is the next the guest lacks: admitted to
    /// the store, into a partial folder of its own, when it's named and the room a stage needs stays free
    /// ([`admits`]); else into the box's `layers/<i>`. Either way it applies over the layers below it. In a
    /// root of one copy, it applies in place, into `path`, a directory already staged, with nothing below.
    fn layer(&mut self, path: &str) -> Result<(), Refusal> {
        self.end_layer()?;
        if self.layers.is_none() {
            // A root of one copy (a bundle's, a `Rootfs::Dir`'s) is one tarball: one layer.
            if self.seq > 0 {
                return Err(invalid(
                    "a root staged as one copy is one layer; a stage of more names them".to_owned(),
                ));
            }
            self.seq = 1;
            let fd = open_in(&self.root, path)?;
            let base = fd_path(&fd)?;
            self.layer = Some(Layer {
                path: path.to_owned(),
                fd,
                base,
                lowers: Vec::new(),
                index: 0,
                overlay: false,
                held: Spend::default(),
                store: None,
            });
            return Ok(());
        }
        let i = self
            .absent
            .pop_front()
            .ok_or_else(|| invalid("the stage sent more layers than the guest lacks".to_owned()))?;
        let digest = self
            .wanted
            .get(i)
            .cloned()
            .ok_or_else(|| invalid("a layer out of the stage's list".to_owned()))?;
        self.seq = self.seq.saturating_add(1);
        let admitted = match digest {
            Some(d) if self.admitting && self.admits_now(Spend::default())? => Some(Pending {
                partial: format!("{PARTIAL}{}-{}-{d}", std::process::id(), self.seq),
                digest: d,
            }),
            Some(_) | None => None,
        };
        let Some(dir) = self.layers.as_ref() else {
            return Err(invalid("a layer of a root staged as one copy".to_owned()));
        };
        let (parent, name) = match &admitted {
            Some(pending) => (&self.store, pending.partial.clone()),
            None => (dir, i.to_string()),
        };
        self.room(|| rustix::fs::mkdirat(parent, name.as_str(), Mode::from_raw_mode(OWNER_DIR)))
            .map_err(os("making a layer's folder"))?;
        let fd = rustix::fs::openat(
            parent,
            name.as_str(),
            OFlags::DIRECTORY | OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(os("opening a layer's folder"))?;
        let base = fd_path(&fd)?;
        // The layers below, nearest first, each by the box's own name for it: a link to the store's
        // (followed: the relay made it) or the box's folder.
        let mut lowers = Vec::with_capacity(i);
        for below in (0..i).rev() {
            lowers.push(
                rustix::fs::openat(
                    dir,
                    below.to_string().as_str(),
                    OFlags::DIRECTORY | OFlags::RDONLY | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(os("opening a layer below"))?,
            );
        }
        self.layer = Some(Layer {
            path: path.to_owned(),
            fd,
            base,
            lowers,
            index: i,
            overlay: true,
            held: Spend::default(),
            store: admitted,
        });
        Ok(())
    }

    /// Finishes the layer under way: its directories' modes and times, and, admitted to the store, its
    /// commit: its size beside it, its partial folder renamed to its digest, whole, and the box's link to it.
    /// Another stage's commit of the same layer first leaves this one's partial to go.
    fn end_layer(&mut self) -> Result<(), Refusal> {
        self.finish_dirs()?;
        let Some(layer) = self.layer.take() else {
            return Ok(());
        };
        if !layer.overlay {
            return Ok(());
        }
        rustix::fs::fchmod(&layer.fd, Mode::from_raw_mode(ROOT_MODE.into()))
            .map_err(os("setting a layer's root mode"))?;
        let Some(pending) = layer.store else {
            return Ok(());
        };
        write_whole(
            &self.store,
            &format!("{}{SIZE}", pending.digest),
            &format!("{} {}\n", layer.held.entries, layer.held.bytes),
        )?;
        match rustix::fs::renameat(
            &self.store,
            pending.partial.as_str(),
            &self.store,
            pending.digest.as_str(),
        ) {
            Ok(()) => {}
            Err(Errno::EXIST | Errno::NOTEMPTY) => {
                let mut budget = fs_budget(&self.store);
                remove_tree(&self.store, &pending.partial, &mut budget)?;
            }
            Err(e) => return Err(Refusal::os("committing a staged layer", e)),
        }
        let _ = rustix::fs::utimensat(
            &self.store,
            pending.digest.as_str(),
            &now(),
            AtFlags::SYMLINK_NOFOLLOW,
        );
        if let Some(dir) = self.layers.as_ref() {
            link_layer(dir, layer.index, &pending.digest)?;
        }
        Ok(())
    }

    /// Removes `leaf` under `parent`, resolved as `at`, with everything under it: nothing when it isn't
    /// there, as containerd's `os.RemoveAll` (`tar.go:218-222,304-311`).
    fn remove(&mut self, parent: &OwnedFd, leaf: &str, at: &str) -> Result<(), Refusal> {
        let kind = match rustix::fs::statat(parent, leaf, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => FileType::from_raw_mode(st.st_mode),
            Err(Errno::NOENT) => return Ok(()),
            Err(e) => return Err(Refusal::os("reading a staged entry's kind", e)),
        };
        self.removals.spend()?;
        if kind == FileType::Directory {
            remove_tree(parent, leaf, &mut self.removals)?;
        } else {
            rustix::fs::unlinkat(parent, leaf, AtFlags::empty())
                .map_err(os("removing a staged file"))?;
        }
        // Its directories' modes and times, and theirs under it, are no longer set.
        let base = if self.layer.is_some() {
            Base::Layer
        } else {
            Base::Root
        };
        let under = format!("{at}/");
        self.dirs.remove(&(base, at.to_owned()));
        let gone: Vec<(Base, String)> = self
            .dirs
            .range((base, under.clone())..)
            .map(|(k, _)| k)
            .take_while(|(b, k)| *b == base && k.starts_with(&under))
            .cloned()
            .collect();
        for k in gone {
            self.dirs.remove(&k);
        }
        Ok(())
    }

    /// An opaque marker. In the store, overlayfs's own: the directory gets the opaque xattr, hiding what
    /// the layers below hold there (containerd's `OverlayConvertWhiteout`; overlayfs.rst). In place, with
    /// nothing below, it hides nothing.
    fn opaque(&mut self, path: &str) -> Result<(), Refusal> {
        let Some(rel) = self.in_layer(path).map(|(_, rel)| rel.to_owned()) else {
            return Err(invalid(format!("opaque marker {path:?} is in no layer")));
        };
        if self.layer.as_ref().is_none_or(|l| !l.overlay) {
            return Ok(());
        }
        let dir = self.walk(&rel, true)?;
        let name = self.opaque_xattr;
        rustix::fs::fsetxattr(&dir, name, b"y", rustix::fs::XattrFlags::empty())
            .map_err(|e| refused_xattr(path, name, e))
    }

    fn data(&mut self, bytes: &[u8], done: bool) -> Result<(), Refusal> {
        let mut rest = bytes;
        while !rest.is_empty() {
            let (file, ..) = self
                .file
                .as_mut()
                .ok_or_else(|| invalid("file bytes came with no file under way".to_owned()))?;
            match io::Write::write(file, rest) {
                Ok(0) => return Err(invalid("a staged file took no bytes".to_owned())),
                Ok(n) => rest = rest.get(n..).unwrap_or_default(),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                // Out of room: a last resort takes an unreferenced layer and the write is tried again.
                Err(e) if e.raw_os_error() == Some(Errno::NOSPC.raw_os_error()) => {
                    if evict_one(&self.staged, &self.store)?.is_none() && !finish_gone(&self.store)?
                    {
                        return Err(Refusal::os("writing a staged file", e));
                    }
                }
                Err(e) => return Err(Refusal::os("writing a staged file", e)),
            }
        }
        if done && let Some((file, path, mode, meta)) = self.file.take() {
            let fd = OwnedFd::from(file);
            apply(&fd, &path, mode, meta.as_ref(), true)?;
        }
        Ok(())
    }

    /// Sets each directory's mode and times, deepest first (a name sorts before those under it, so in
    /// reverse they come first) so a directory made read-only doesn't stop its children's and writing
    /// them doesn't move its time.
    fn finish_dirs(&mut self) -> Result<(), Refusal> {
        let dirs = std::mem::take(&mut self.dirs);
        for ((base, path), (mode, times)) in dirs.iter().rev() {
            let top = match base {
                Base::Root => &self.root,
                Base::Layer => match self.layer.as_ref() {
                    Some(layer) => &layer.fd,
                    None => continue,
                },
            };
            let dir = open_in(top, if path.is_empty() { "." } else { path })?;
            rustix::fs::fchmod(&dir, Mode::from_raw_mode((*mode).into()))
                .map_err(os("setting a staged directory's mode"))?;
            if let Some(times) = times {
                rustix::fs::futimens(&dir, times)
                    .map_err(os("setting a staged directory's times"))?;
            }
        }
        Ok(())
    }

    /// Finishes the last layer, and the stage: every layer the guest lacked sent, and the root's mode. What
    /// it took of its file system (the drop in its free room since it began) raises the room the store
    /// leaves free for a stage, never lowers it.
    fn end(&mut self) -> Result<(), Refusal> {
        self.end_layer()?;
        if !self.absent.is_empty() {
            return Err(invalid(format!(
                "the stage ended {} layers short of what the guest lacks",
                self.absent.len()
            )));
        }
        rustix::fs::fchmod(&self.root, Mode::from_raw_mode(ROOT_MODE.into()))
            .map_err(os("setting a staged root's mode"))?;
        if self.layers.is_some() {
            let taken = taken(self.free, (self.read_free)(&self.staged)?);
            self.reads = self.reads.saturating_add(1);
            if taken.entries <= self.need.entries && taken.bytes <= self.need.bytes {
                return Ok(());
            }
            let _lock = lock(&self.store)?;
            self.locks = self.locks.saturating_add(1);
            let need = read_spend(&self.store, NEED);
            let rises = Spend {
                entries: need.entries.max(taken.entries),
                bytes: need.bytes.max(taken.bytes),
            };
            if rises != need {
                write_whole(
                    &self.store,
                    NEED,
                    &format!("{} {}\n", rises.entries, rises.bytes),
                )?;
            }
        }
        Ok(())
    }

    fn abandon(mut self) -> Result<(), Refusal> {
        drop(self.file.take());
        if let Some(pending) = self.layer.take().and_then(|l| l.store) {
            let mut budget = fs_budget(&self.store);
            let _ = remove_tree(&self.store, &pending.partial, &mut budget);
        }
        let mut budget = Budget::new(self.entries.saturating_add(1));
        remove_tree(&self.staged, self.id.as_str(), &mut budget)
    }
}

/// Whether the store may take a stage's next `entry`: what its file system has free `now`, less the entry,
/// still holds the largest stage learned (`need`, what a stage has taken of the file system, not its files'
/// bytes: a tmpfs charges whole pages a file), so the next create finds the room it needs without evicting.
/// A stage larger than any learned, and a guest's first, can still crowd the next: that one's is the last
/// resort ([`Stage::room`]), once, and it raises what's learned.
fn admits(now: Free, entry: Spend, need: Spend) -> bool {
    now.entries.saturating_sub(entry.entries) >= need.entries
        && now.bytes.saturating_sub(entry.bytes) >= need.bytes
}

/// What a file system's free room fell by between `begin` and `now`.
fn taken(begin: Free, now: Free) -> Spend {
    Spend {
        entries: begin.entries.saturating_sub(now.entries),
        bytes: begin.bytes.saturating_sub(now.bytes),
    }
}

/// An entry of `bytes` (a file's and its xattrs'): one entry, its bytes in whole blocks of `block`.
fn charge(bytes: u64, block: u64) -> Spend {
    Spend {
        entries: 1,
        bytes: bytes.div_ceil(block.max(1)).saturating_mul(block.max(1)),
    }
}

/// The box's `layers/<i>`, a link to the store's layer `digest` (`../../.layers/<digest>`, relative, so it
/// names the store wherever the staged folder is).
fn link_layer(dir: &OwnedFd, i: usize, digest: &str) -> Result<(), Refusal> {
    rustix::fs::symlinkat(
        format!("../../{STORE}/{digest}").as_str(),
        dir,
        i.to_string().as_str(),
    )
    .map_err(os("linking a box's layer to the store"))
}

/// The layer store under `staged`, made if it isn't there.
fn open_store(staged: &OwnedFd) -> Result<OwnedFd, Refusal> {
    match rustix::fs::mkdirat(staged, STORE, Mode::from_raw_mode(OWNER_DIR)) {
        Ok(()) | Err(Errno::EXIST) => {}
        Err(e) => return Err(Refusal::os("making the layer store", e)),
    }
    rustix::fs::openat(
        staged,
        STORE,
        OFlags::DIRECTORY | OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(os("opening the layer store"))
}

/// The store's lock (flock(2) on `.layers/.lock`), held while a stage records its layers and counts which
/// the store holds, while one raises the room a stage needs, and while a stage out of room takes a layer out
/// of the store: so an eviction never takes a layer a stage counted as held. Each holds it for a bounded few
/// metadata calls, never for a layer's bytes; it's released when the guard drops.
fn lock(store: &OwnedFd) -> Result<OwnedFd, Refusal> {
    let fd = rustix::fs::openat(
        store,
        LOCK,
        OFlags::CREATE | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(OWNER_FILE),
    )
    .map_err(os("opening the layer store's lock"))?;
    rustix::fs::flock(&fd, rustix::fs::FlockOperation::LockExclusive)
        .map_err(os("taking the layer store's lock"))?;
    Ok(fd)
}

/// The entries and bytes a store's file `name` says (`<entries> <bytes>`), or none when it isn't there.
fn read_spend(store: &OwnedFd, name: &str) -> Spend {
    let Ok(fd) = rustix::fs::openat(
        store,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) else {
        return Spend::default();
    };
    let mut text = String::new();
    if io::Read::read_to_string(&mut std::fs::File::from(fd), &mut text).is_err() {
        return Spend::default();
    }
    let mut parts = text
        .split_whitespace()
        .map(|p| p.parse::<u64>().unwrap_or(0));
    Spend {
        entries: parts.next().unwrap_or(0),
        bytes: parts.next().unwrap_or(0),
    }
}

/// Layers taken out of the store by this process, counted, so each `.gone-<pid>-<n>` is its own.
static GONE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Takes the least recently used layer no box's record names out of the store (under its lock, renamed to
/// `.gone-<pid>-<n>`, so no stage counts it as held after), then removes it; returns what its size file said
/// it held, or none when there is no such layer.
fn evict_one(staged: &OwnedFd, store: &OwnedFd) -> Result<Option<Spend>, Refusal> {
    let gone = format!(
        "{GONE}{}-{}",
        std::process::id(),
        GONE_SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let taken = {
        let _lock = lock(store)?;
        let held = referenced(staged)?;
        let mut oldest: Option<((i64, i64), String)> = None;
        for name in names(store)? {
            if !is_layer_name(&name) || held.contains(&name) {
                continue;
            }
            let Ok(st) = rustix::fs::statat(store, name.as_str(), AtFlags::SYMLINK_NOFOLLOW) else {
                continue;
            };
            let when = (st.st_mtime, i64::try_from(st.st_mtime_nsec).unwrap_or(0));
            if oldest.as_ref().is_none_or(|(w, _)| when < *w) {
                oldest = Some((when, name));
            }
        }
        let Some((_, name)) = oldest else {
            return Ok(None);
        };
        let size = read_spend(store, &format!("{name}{SIZE}"));
        rustix::fs::renameat(store, name.as_str(), store, gone.as_str())
            .map_err(os("taking a layer out of the store"))?;
        // Its size goes with it, so a budget counts its room until it's free.
        let _ = rustix::fs::renameat(
            store,
            format!("{name}{SIZE}").as_str(),
            store,
            format!("{gone}{SIZE}").as_str(),
        );
        // Held while it's removed: a stage out of room waits on it ([`finish_gone`]).
        let remover = gone_lock(store, &gone)?;
        (size, remover)
    };
    let (size, _remover) = taken;
    let mut budget = fs_budget(store);
    remove_tree(store, &gone, &mut budget)?;
    let _ = rustix::fs::unlinkat(store, format!("{gone}{SIZE}").as_str(), AtFlags::empty());
    Ok(Some(size))
}

/// The lock on a layer being taken out of the store (flock(2) on its `.gone-` folder), held by its remover.
fn gone_lock(store: &OwnedFd, gone: &str) -> Result<OwnedFd, Refusal> {
    let fd = rustix::fs::openat(
        store,
        gone,
        OFlags::DIRECTORY | OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(os("opening a layer being taken out"))?;
    rustix::fs::flock(&fd, rustix::fs::FlockOperation::LockExclusive)
        .map_err(os("locking a layer being taken out"))?;
    Ok(fd)
}

/// For a stage out of room with no layer left to take: waits for each layer another thread is taking out
/// of the store to be gone (its remover's lock; bounded by that removal), and finishes one whose remover
/// ended part way. Whether there was any, and so room may now be free.
fn finish_gone(store: &OwnedFd) -> Result<bool, Refusal> {
    let mut any = false;
    for name in names(store)? {
        if !name.starts_with(GONE) || name.ends_with(SIZE) {
            continue;
        }
        any = true;
        let Ok(_waited) = gone_lock(store, &name) else {
            // Gone already: its room is free.
            continue;
        };
        if rustix::fs::statat(store, name.as_str(), AtFlags::SYMLINK_NOFOLLOW).is_ok() {
            let mut budget = fs_budget(store);
            remove_tree(store, &name, &mut budget)?;
            let _ = rustix::fs::unlinkat(store, format!("{name}{SIZE}").as_str(), AtFlags::empty());
        }
    }
    Ok(any)
}

/// What mkparent copies to a directory it makes from the one below (`copyDirInfo`, `copyUpXAttrs`,
/// containerd `pkg/archive/tar_unix.go:143-203`): owner, mode, atime and mtime, and every xattr but
/// `trusted.*`.
fn copy_dir_info(from: &OwnedFd, to: &OwnedFd) -> Result<(), Refusal> {
    let st = rustix::fs::fstat(from).map_err(os("reading a lower directory's attributes"))?;
    rustix::fs::fchown(
        to,
        Some(Uid::from_raw(st.st_uid)),
        Some(Gid::from_raw(st.st_gid)),
    )
    .map_err(os("copying a lower directory's owner"))?;
    rustix::fs::fchmod(
        to,
        Mode::from_raw_mode(st.st_mode & u32::from(files::MODE_BITS)),
    )
    .map_err(os("copying a lower directory's mode"))?;
    let mut names = vec![0u8; XATTR_LIST_MAX];
    let n = match rustix::fs::flistxattr(from, &mut names) {
        Ok(n) => n,
        Err(Errno::NOTSUP) => 0,
        Err(e) => return Err(Refusal::os("listing a lower directory's xattrs", e)),
    };
    let mut value = vec![0u8; XATTR_SIZE_MAX];
    for name in names.get(..n).unwrap_or_default().split(|b| *b == 0) {
        if name.is_empty() || name.starts_with(b"trusted.") {
            continue;
        }
        let len = match rustix::fs::fgetxattr(from, name, &mut value) {
            Ok(len) => len,
            Err(Errno::NODATA | Errno::NOTSUP) => continue,
            Err(e) => return Err(Refusal::os("reading a lower directory's xattr", e)),
        };
        rustix::fs::fsetxattr(
            to,
            name,
            value.get(..len).unwrap_or_default(),
            rustix::fs::XattrFlags::empty(),
        )
        .map_err(os("copying a lower directory's xattr"))?;
    }
    rustix::fs::futimens(
        to,
        &Timestamps {
            last_access: Timespec {
                tv_sec: st.st_atime,
                tv_nsec: st.st_atime_nsec.try_into().unwrap_or(0),
            },
            last_modification: Timespec {
                tv_sec: st.st_mtime,
                tv_nsec: st.st_mtime_nsec.try_into().unwrap_or(0),
            },
        },
    )
    .map_err(os("copying a lower directory's times"))
}

/// The path the kernel holds for the directory `fd` (proc(5), `/proc/self/fd/<n>`).
fn fd_path(fd: &OwnedFd) -> Result<String, Refusal> {
    let link = format!("/proc/self/fd/{}", fd.as_raw_fd());
    let path = rustix::fs::readlink(link.as_str(), Vec::new())
        .map_err(os("reading a staged directory's path"))?;
    path.into_string()
        .map_err(|_| invalid("a staged directory's path isn't UTF-8".to_owned()))
}

/// `path` within `root`, a layer's root, opened through every link in it, an absolute one or a `..`
/// resolved inside `root` (`RESOLVE_IN_ROOT`, openat2(2)), through no magic link or mount: what an earlier
/// layer's name resolves to in the layers below, whose attributes mkparent copies (`WithParents`,
/// `pkg/archive/tar.go:482-500`). `""` is `root` itself.
fn within(root: &OwnedFd, path: &str) -> Result<OwnedFd, Errno> {
    let path = if path.is_empty() { "." } else { path };
    let mut tries = 0;
    loop {
        match rustix::fs::openat2(
            root,
            path,
            OFlags::DIRECTORY | OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
            WITHIN,
        ) {
            Err(Errno::AGAIN) if tries < RESOLVE_TRIES => tries = tries.saturating_add(1),
            done => return done,
        }
    }
}

/// `path` under `root`, a directory, opened inside it through no symbolic link.
fn open_in(root: &OwnedFd, path: &str) -> Result<OwnedFd, Refusal> {
    let mut tries = 0;
    loop {
        match rustix::fs::openat2(
            root,
            path,
            OFlags::DIRECTORY | OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
            RESOLVE,
        ) {
            Ok(fd) => return Ok(fd),
            Err(Errno::AGAIN) if tries < RESOLVE_TRIES => tries = tries.saturating_add(1),
            Err(Errno::XDEV) => {
                return Err(Refusal::Invalid {
                    what: "staged files",
                    why: format!("{path:?} leads through a mount"),
                });
            }
            // A symbolic link in the way is `ELOOP` from openat2's RESOLVE_NO_SYMLINKS, but the
            // last component's, opened O_DIRECTORY|O_NOFOLLOW, is `ENOTDIR` on Linux 6.19 (as a
            // file in the way is): which is read from the component itself.
            Err(e @ (Errno::LOOP | Errno::NOTDIR)) => return Err(in_the_way(root, path, e)),
            Err(e) => {
                return Err(Refusal::Os {
                    op: "opening a staged directory",
                    why: format!("{path:?}: {e}"),
                });
            }
        }
    }
}

/// Why `path` under `root` isn't a directory to open, `err` being the open's: its first component that
/// is a symbolic link or not a directory, read without following it, each prefix before it a directory
/// already (so each read passes through directories alone). At most the path's components are read.
fn in_the_way(root: &OwnedFd, path: &str, err: Errno) -> Refusal {
    let mut end: usize = 0;
    for part in path.split('/') {
        end = end.saturating_add(part.len());
        let prefix = path.get(..end).unwrap_or(path);
        end = end.saturating_add(1);
        let kind = rustix::fs::statat(root, prefix, AtFlags::SYMLINK_NOFOLLOW)
            .map(|st| FileType::from_raw_mode(st.st_mode));
        let why = match kind {
            Ok(FileType::Directory) => continue,
            Ok(FileType::Symlink) => format!("{path:?} leads through a symbolic link, {prefix:?}"),
            Ok(_) => format!("{path:?} leads through {prefix:?}, which isn't a directory"),
            Err(_) => break,
        };
        return Refusal::Invalid {
            what: "staged files",
            why,
        };
    }
    Refusal::Os {
        op: "opening a staged directory",
        why: format!("{path:?}: {err}"),
    }
}

/// Removes `<staged>/<id>` and everything under it.
pub fn remove(staged: &Path, id: &BoxId) -> Result<(), Refusal> {
    let staged = rustix::fs::open(
        staged,
        OFlags::DIRECTORY | OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(os("opening the staged folder"))?;
    // A removal visits no more entries than the file system holds.
    let mut budget = Budget::new(usize::try_from(free(&staged)?.used).unwrap_or(usize::MAX));
    match rustix::fs::statat(&staged, id.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
        Err(Errno::NOENT) => return Err(Refusal::NoSuchBox(id.clone())),
        Err(e) => return Err(Refusal::os("opening a box's staged root", e)),
        Ok(_) => {}
    }
    remove_tree(&staged, id.as_str(), &mut budget)
}

/// Removes the directory `leaf` under `parent` and everything under it, each entry visited spending
/// `budget`: flat, a list of the directories found, emptied deepest first (CLAUDE.md §11).
fn remove_tree(parent: &OwnedFd, leaf: &str, budget: &mut Budget) -> Result<(), Refusal> {
    let root = rustix::fs::openat(
        parent,
        leaf,
        OFlags::DIRECTORY | OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(os("opening a staged directory to removal"))?;
    // Every directory under the root, parents before children; the root's own is ".".
    let mut dirs: Vec<String> = vec![".".to_owned()];
    let mut next = 0;
    while let Some(dir) = dirs.get(next).cloned() {
        next = next.saturating_add(1);
        let fd = open_in(&root, &dir)?;
        // A directory may be read-only (its staged mode, or the container's): the relay empties it.
        rustix::fs::fchmod(&fd, Mode::from_raw_mode(OWNER_DIR))
            .map_err(os("opening a staged directory to removal"))?;
        let reader = Dir::read_from(&fd).map_err(os("listing a staged directory"))?;
        for entry in reader {
            let entry = entry.map_err(os("listing a staged directory"))?;
            let name = entry.file_name();
            if name == c"." || name == c".." {
                continue;
            }
            budget.spend()?;
            let kind = match entry.file_type() {
                FileType::Unknown => rustix::fs::statat(&fd, name, AtFlags::SYMLINK_NOFOLLOW)
                    .map(|st| FileType::from_raw_mode(st.st_mode))
                    .map_err(os("reading a staged entry's kind"))?,
                kind => kind,
            };
            if kind == FileType::Directory {
                let name = name
                    .to_str()
                    .map_err(|_| invalid(format!("a name under {dir:?} isn't UTF-8")))?;
                dirs.push(if dir == "." {
                    name.to_owned()
                } else {
                    format!("{dir}/{name}")
                });
            } else {
                rustix::fs::unlinkat(&fd, name, AtFlags::empty())
                    .map_err(os("removing a staged file"))?;
            }
        }
    }
    for dir in dirs.iter().skip(1).rev() {
        let (up, name) = dir.rsplit_once('/').unwrap_or((".", dir.as_str()));
        let up = open_in(&root, up)?;
        rustix::fs::unlinkat(&up, name, AtFlags::REMOVEDIR)
            .map_err(os("removing a staged directory"))?;
    }
    drop(root);
    rustix::fs::unlinkat(parent, leaf, AtFlags::REMOVEDIR)
        .map_err(os("removing a staged directory"))?;
    Ok(())
}

#[cfg(test)]
mod free_tests;
#[cfg(test)]
mod store_tests;
