//! The host's side of staging (docs/design/shards-guest.md §4a): a box's bundle and root, host paths
//! the guest can't see, packed onto the guest's staging port ([`files::FILES_PORT`]) before its create,
//! and their copy removed after its delete. The bytes are the core's ([`hex_core::stream::files`]); this
//! walks the host's files, flat and counted (CLAUDE.md §2, §11), and refuses by name what is never
//! staged: a device node, a FIFO, a socket, a name that isn't UTF-8.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, BufRead, BufWriter, Read, Write};
use std::net::Shutdown;
use std::path::{Path, PathBuf};

use hex_api::{BoxId, Refusal};
use hex_core::bundle::MAX_LAYERS;
use hex_core::patience::Patience;
use hex_core::stream::Deframer;
use hex_core::stream::files::{self, Entry, Limits, Meta, Mtime, Op, What, Xattr};
use hex_core::wire::{self, Answer, Received};

use super::{DuplexOwner, Link};

/// How long a staging connection waits on the guest's relay, each read for its answer and each write for room
/// (decision 9): W*, the slowest answer to a stage's request this process has had, from its last byte sent to the
/// relay's answer, a bound only while answers come within it ([`Staging`]). Before its first answer, and from a
/// run-out to the next, a staging connection waits for its relay alone, as long as its connection stands. No record is
/// read: the `guest.stage` rows of `bench/competitive/probes/waits_census.md` are evidence.
static STAGING: Patience = Patience::measuring("guest.stage");

/// A staging connection, its waits on the relay as decision 9 waits them (the person's ruling of 2026-11-13): each read
/// and each write within W* ([`STAGING`], or a test's own). Past it the run-out is told to the census, once and with no
/// sample, and the connection is followed by its relay's life alone, as at every local site
/// (docs/research/runtime/learned-waits.md), its timeouts let go: only the connection's end or reset ends a read or a
/// write then.
///
/// With no W* each waits for the relay alone. The answer, however long it took, is the sample W* rises to, and the
/// connection's next waits are bounded again, by the W* it rose to ([`Staging::answered`]). Its timeouts are set at its
/// next read or write, never on a connection its relay closed after its last answer: macOS refuses a closed socket's
/// options (`EINVAL`).
struct Staging<'a> {
    conn: DuplexOwner,
    wait: &'a Patience,
    /// W*, the connection's timeouts while within it: none with no W*, and none past it.
    window: Option<std::time::Duration>,
    /// Whether the connection's timeouts are `window`'s: false from a change of it to the next read or write.
    armed: bool,
}

impl Staging<'_> {
    /// Within W* as `wait` has it now (none with none), from the next read or write.
    fn within(&mut self) {
        self.window = self.wait.deadline();
        self.armed = false;
    }

    /// An answer of the relay's, `took` after what it answers was sent: the sample W* rises to, and the connection's
    /// next waits bounded by it.
    fn answered(&mut self, took: std::time::Duration) {
        self.wait.answered(took);
        self.within();
    }

    /// Sets the connection's timeouts to its window, where they aren't already.
    fn arm(&mut self) -> io::Result<()> {
        if !self.armed {
            self.conn.set_write_timeout(self.window)?;
            self.conn.set_read_timeout(self.window)?;
            self.armed = true;
        }
        Ok(())
    }

    /// A read or a write, begun at `began`, whose timeout ended: once W*, `window`, has passed since it began, past W*
    /// from now on, its run-out told, once, and the connection followed by its relay's life alone, its timeouts let go
    /// until an answer bounds it again. A socket's timeout counts the kernel's ticks, and can end before its window
    /// has passed (a timeout of one tick ends at the next): such a read or write is only tried again.
    fn timed_out(
        &mut self,
        began: std::time::Instant,
        window: std::time::Duration,
    ) -> io::Result<()> {
        if began.elapsed() < window {
            return Ok(());
        }
        self.wait.ran_out(window);
        self.window = None;
        self.armed = false;
        self.arm()
    }

    fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        self.conn.shutdown(how)
    }
}

/// Whether `e` is a timeout's end of a read or a write.
fn timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

impl Read for Staging<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.arm()?;
        let began = std::time::Instant::now();
        loop {
            match (self.conn.read(buf), self.window) {
                (Err(e), Some(window)) if timeout(&e) => self.timed_out(began, window)?,
                (got, _) => return got,
            }
        }
    }
}

impl Write for Staging<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.arm()?;
        let began = std::time::Instant::now();
        loop {
            match (self.conn.write(buf), self.window) {
                (Err(e), Some(window)) if timeout(&e) => self.timed_out(began, window)?,
                (got, _) => return got,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.conn.flush()
    }
}

/// Reads the relay's answer to what was sent at `sent`, the time it took taught to the connection's wait.
fn relay_answer(
    conn: &mut Staging<'_>,
    deframer: &mut Deframer,
    sent: std::time::Instant,
) -> Result<Vec<u8>, Refusal> {
    let got = message(conn, deframer)?;
    conn.answered(sent.elapsed());
    Ok(got)
}

/// Where the guest's relay stages each box: `<STAGED>/<box id>/` (`hex_node::relay::STAGED`).
pub const STAGED: &str = "/run/hex/staged";

/// One thing to stage, at the top of the box's staged root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// Its name under the staged root: a plain name or a relative path.
    pub name: String,
    pub source: Source,
}

/// What an item is made from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A host path, a directory with everything under it, a file or a symbolic link.
    Path(PathBuf),
    /// A file of these bytes (a bundle's rewritten `config.json`), mode 0644.
    Bytes(Vec<u8>),
    /// A tarball (`.tar`, or gzip's `.tar.gz` or `.tgz`), staged from its own entries: their Linux modes
    /// and symlink texts exact, which a root unpacked on a host that keeps neither would lose (§4a).
    Tar(PathBuf),
    /// An image's layers, each a tarball, lowest first, each applied as containerd's overlay applier applies
    /// a layer and mounted together by overlayfs (§4a): into the guest's layer store by its digest where the
    /// store has room, else into the box's own `layers/<i>`. A layer the guest holds isn't sent again.
    Layers(Vec<Layer>),
}

/// One layer of an image: its tarball, and its digest, the store's name for it, when the host knows it (its
/// blob's name, or a digest learned when it last crossed: [`Learned`]). A layer of no known digest is staged
/// into the box, hashed as it crosses, its digest learned for the next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layer {
    pub path: PathBuf,
    pub digest: Option<files::Digest>,
}

/// What identifies a file's bytes without reading them: on Unix its device and inode, size, and modification
/// and status-change times to the nanosecond (stat(2)); on Windows its size and its last-write and creation
/// times. A digest learned for a file is trusted only while its identity is the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

/// `meta`'s file's identity.
#[cfg(unix)]
pub fn identity(meta: &fs::Metadata) -> Identity {
    use std::os::unix::fs::MetadataExt;
    Identity {
        dev: meta.dev(),
        ino: meta.ino(),
        size: meta.size(),
        modified: (meta.mtime(), meta.mtime_nsec()),
        changed: (meta.ctime(), meta.ctime_nsec()),
    }
}

/// `meta`'s file's identity.
#[cfg(windows)]
pub fn identity(meta: &fs::Metadata) -> Identity {
    use std::os::windows::fs::MetadataExt;
    let at = |t: u64| (i64::try_from(t).unwrap_or(i64::MAX), 0);
    Identity {
        size: meta.file_size(),
        modified: at(meta.last_write_time()),
        changed: at(meta.creation_time()),
    }
}

/// A layer of no known digest, hashed as it crossed: its file, its digest, and its identity, the same when
/// the packer opened it and when it had read it to its end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Learned {
    pub path: PathBuf,
    pub digest: files::Digest,
    pub identity: Identity,
}

/// What a stage learned: the digests of the layers it hashed, and how many layers the guest's store can hold
/// ([`files::Budget::capacity`]), what the host bounds its memory of digests by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Staged {
    pub learned: Vec<Learned>,
    pub capacity: u64,
}

/// A layer blob's digest, from its name as its image keeps it: `…/sha256/<64 hex digits>`, an OCI image
/// layout's `blobs/<alg>/<encoded>` (image-spec image-layout.md, "Blobs") and containerd's content store's
/// alike. A create takes a layer's digest from its name and never reads the layer to learn it; a layer the
/// guest lacks is hashed as it is sent, and refused unless its bytes are that digest (§4a).
pub fn blob_digest(path: &Path) -> Option<files::Digest> {
    let alg = path.parent()?.file_name()?;
    let name = path.file_name()?.to_str()?;
    if alg != "sha256" || name.len() != files::DIGEST_BYTES.checked_mul(2)? {
        return None;
    }
    let mut digest = [0u8; files::DIGEST_BYTES];
    for (byte, pair) in digest.iter_mut().zip(name.as_bytes().chunks(2)) {
        let text = std::str::from_utf8(pair).ok()?;
        // Lowercase alone, as the spec's sha256 encoding is (descriptor.md, "Digests").
        if text.bytes().any(|b| b.is_ascii_uppercase()) {
            return None;
        }
        *byte = u8::from_str_radix(text, 16).ok()?;
    }
    Some(digest)
}

/// The guest's path of `id`'s `i`th layer, lowest first, the relay staging under `staged`: the box's own
/// `layers/<i>`, a link to the store's folder for a layer the store holds, or the layer itself, staged into
/// the box (§4a). A box's overlay names its layers so, whichever each is.
pub fn box_layer_path(staged: &str, id: &BoxId, i: usize) -> String {
    format!("{staged}/{id}/layers/{i}")
}

/// Whether `path` names a tarball, by its name: `.tar`, `.tar.gz` or `.tgz`, or a layer blob named by its
/// digest ([`blob_digest`]).
pub fn is_tarball(path: &Path) -> bool {
    blob_digest(path).is_some()
        || path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(".tar") || n.ends_with(".tar.gz") || n.ends_with(".tgz"))
}

/// The guest's path of `name` under `id`'s staged root, the relay staging under `staged`.
pub fn staged_path(staged: &str, id: &BoxId, name: &str) -> String {
    format!("{staged}/{id}/{name}")
}

/// Stages `items` for `id` through `link`, waiting for the relay's answer.
pub fn stage(
    link: &dyn Link,
    id: &BoxId,
    items: &[Item],
    limits: &Limits,
) -> Result<Staged, Refusal> {
    stage_with(link, id, items, limits, &STAGING)
}

/// [`stage`], its waits on the relay as `wait` has learned them.
fn stage_with(
    link: &dyn Link,
    id: &BoxId,
    items: &[Item],
    limits: &Limits,
    wait: &Patience,
) -> Result<Staged, Refusal> {
    // The root's layers, by digest: the store's names for them (§4a). A stage stages one root.
    let layers: Vec<&Layer> = items
        .iter()
        .filter_map(|item| match &item.source {
            Source::Layers(layers) => Some(layers),
            _ => None,
        })
        .flatten()
        .collect();
    let digests: Vec<Option<files::Digest>> = layers.iter().map(|l| l.digest).collect();
    let mut conn = dial(link, wait)?;
    let mut deframer = Deframer::new(wire::MAX_MESSAGE);
    conn.write_all(&files::open_stage(id.as_str(), &digests).map_err(refused)?)
        .map_err(|e| Refusal::os("sending staged files", e))?;
    // The relay answers the opening with the stage's budget, what its file system has free and which of
    // the layers it already holds, or refuses.
    let first = relay_answer(&mut conn, &mut deframer, std::time::Instant::now())?;
    let Some(budget) = files::read_budget(&first) else {
        return Err(answer_of(&first).err().unwrap_or_else(|| {
            Refusal::Broken("the relay answered a stage's opening with no budget".to_owned())
        }));
    };
    if budget.present.len() != digests.len() {
        return Err(Refusal::Broken(format!(
            "the relay said which of {} layers it holds, of {}",
            budget.present.len(),
            digests.len()
        )));
    }
    let learned = Limits {
        entries: limits.entries.min(budget.entries),
        bytes: limits.bytes.min(budget.bytes),
        path: limits.path,
    };
    let sent = {
        let mut out = BufWriter::with_capacity(64 << 10, &mut conn);
        pack_items(&mut out, items, &learned, &budget.present, budget.admitting).and_then(
            |hashed| {
                out.write_all(&files::END)
                    .and_then(|()| out.flush())
                    .map(|()| hashed)
                    .map_err(|e| Refusal::os("sending staged files", e))
            },
        )
    };
    // The stream ended in-band at its end head, where the relay answers, so its answer is read with the
    // connection whole: a half-close first is a full shutdown over Firecracker's vsock, which then drops what
    // the guest sends (`vsock/csm/connection.rs:228-239` and `:307` at upstream 500efc6; AC-T7.4.1). A packer
    // that refused part way can't end the stream in-band, its head having promised the file whole, so it ends
    // its sending half: the relay removes its partial copy and answers, the answer coming where the transport
    // carries a half-close. The relay answers even when it refused part way, and its answer says why better
    // than a broken pipe: read it first.
    if sent.is_err() {
        let _ = conn.shutdown(Shutdown::Write);
    }
    let ended = std::time::Instant::now();
    // The answer is read first, so the relay has removed a partial copy before this returns. Its
    // answer names a stream cut short when the packer refused part way; the packer's own refusal names
    // the entry, and is the one returned, unless it was only the connection failing.
    let answered = relay_answer(&mut conn, &mut deframer, ended).and_then(|m| answer_of(&m));
    match (answered, sent) {
        (_, Err(why)) if !matches!(&why, Refusal::Os { op, .. } if op.starts_with("sending")) => {
            Err(why)
        }
        (Ok(()), Ok(learned)) => Ok(Staged {
            learned,
            capacity: budget.capacity,
        }),
        (Err(why), _) | (Ok(()), Err(why)) => Err(why),
    }
}

/// Removes `id`'s staged copy through `link`. A copy that isn't there is [`Refusal::NoSuchBox`].
pub fn unstage(link: &dyn Link, id: &BoxId) -> Result<(), Refusal> {
    let wait = &STAGING;
    let mut conn = dial(link, wait)?;
    let open = files::open(Op::Remove, id.as_str()).map_err(refused)?;
    conn.write_all(&open)
        .map_err(|e| Refusal::os("asking for a staged copy's removal", e))?;
    // A removal ends at its opening, where the relay answers: read with the connection whole, as a stage's is.
    let sent = std::time::Instant::now();
    let mut deframer = Deframer::new(wire::MAX_MESSAGE);
    relay_answer(&mut conn, &mut deframer, sent).and_then(|m| answer_of(&m))
}

/// A staging connection to the relay, its waits as `wait` has learned them ([`Staging`]).
fn dial<'a>(link: &dyn Link, wait: &'a Patience) -> Result<Staging<'a>, Refusal> {
    let mut conn = Staging {
        conn: link.dial(files::FILES_PORT)?,
        wait,
        window: None,
        armed: false,
    };
    conn.within();
    Ok(conn)
}

fn refused(why: files::Refused) -> Refusal {
    Refusal::Invalid {
        what: "staged files",
        why: why.to_string(),
    }
}

/// The relay's next framed message.
fn message(conn: &mut impl Read, deframer: &mut Deframer) -> Result<Vec<u8>, Refusal> {
    // A whole message's bound, so one read can hold an answer.
    let mut buf = vec![0u8; wire::MAX_MESSAGE];
    loop {
        let n = match conn.read(&mut buf) {
            Ok(0) => {
                return Err(Refusal::Broken(
                    "the guest's relay closed a staging connection without an answer".to_owned(),
                ));
            }
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(Refusal::os("reading the relay's staging answer", e)),
        };
        let mut input = buf.get(..n).unwrap_or_default();
        while !input.is_empty() {
            let (took, whole) = deframer.take(input).map_err(|f| {
                Refusal::Broken(format!("the relay's staging answer isn't a message: {f}"))
            })?;
            input = input.get(took..).unwrap_or_default();
            if let Some(message) = whole {
                if !input.is_empty() {
                    return Err(Refusal::Broken(
                        "the relay sent more than one message at once".to_owned(),
                    ));
                }
                return Ok(message);
            }
        }
    }
}

/// The relay's answer in `message`: `Done`, or the refusal it carries.
fn answer_of(message: &[u8]) -> Result<(), Refusal> {
    let received = Received {
        bytes: message,
        size: message.len(),
        descriptors: 0,
        descriptors_cut: false,
    };
    match wire::read_answer(&received, wire::MAX_MESSAGE) {
        Ok((_, Answer::Done)) => Ok(()),
        Ok((_, Answer::Refused(why))) => Err(why),
        Ok((_, other)) => Err(Refusal::Broken(format!(
            "the relay answered a stage with {other:?}"
        ))),
        Err(f) => Err(Refusal::Broken(format!(
            "the relay's staging answer isn't one: {f}"
        ))),
    }
}

/// What the walk refuses: a path of a kind never staged, or one whose name can't be carried.
fn never(path: &Path, why: &str) -> Refusal {
    Refusal::Invalid {
        what: "staged files",
        why: format!("{}: {why}", path.display()),
    }
}

/// The permission bits of a mode (inode(7)): its low 16 bits, masked to [`files::MODE_BITS`].
fn permissions(mode: u32) -> u16 {
    let [low, high, ..] = mode.to_le_bytes();
    u16::from_le_bytes([low, high]) & files::MODE_BITS
}

/// A file's mode, its permission bits.
#[cfg(unix)]
fn mode(meta: &fs::Metadata) -> u16 {
    use std::os::unix::fs::PermissionsExt;
    permissions(meta.permissions().mode())
}

/// NTFS holds no mode, so on Windows every entry is staged as WSL's DrvFs shows a file without metadata:
/// 0777 under its default `umask` of 022, 0755 (learn.microsoft.com/windows/wsl/file-permissions, "Default
/// permissions with no metadata").
#[cfg(not(unix))]
fn mode(_meta: &fs::Metadata) -> u16 {
    0o777 & !0o022
}

/// What kind of file a host path is, or why it's never staged.
enum Kind {
    Dir,
    File(u64),
    Symlink,
}

fn kind(path: &Path, meta: &fs::Metadata) -> Result<Kind, Refusal> {
    let t = meta.file_type();
    if t.is_symlink() {
        return Ok(Kind::Symlink);
    }
    if t.is_dir() {
        return Ok(Kind::Dir);
    }
    if t.is_file() {
        return Ok(Kind::File(meta.len()));
    }
    #[cfg(unix)]
    let what = {
        use std::os::unix::fs::FileTypeExt;
        if t.is_block_device() || t.is_char_device() {
            "a device node is never staged"
        } else if t.is_fifo() {
            "a FIFO is never staged"
        } else if t.is_socket() {
            "a socket is never staged"
        } else {
            "a file of no kind staging carries"
        }
    };
    #[cfg(not(unix))]
    let what = "a file of no kind staging carries";
    Err(never(path, what))
}

/// Writes every entry of `items` to `out`, within `limits`: each directory walked flat, a queue of the
/// directories found (CLAUDE.md §11), and every entry counted against the stage's bound before it is sent.
/// Packs `items`, sending only the layers the guest lacks (`present`). A named layer's bytes are checked
/// against its digest only when the stage is `admitting`: a stage that admits nothing stages every layer
/// into the box, never the store, as whole-root staging did, which checked none (the lead's rule after
/// HEX-GUEST-22's 2x diagnosis: the check cost alpine 1 ms and caddy 6 ms a create). A layer of no known
/// digest is hashed either way, to learn it.
fn pack_items(
    out: &mut dyn Write,
    items: &[Item],
    limits: &Limits,
    present: &[bool],
    admitting: bool,
) -> Result<Vec<Learned>, Refusal> {
    let send = |out: &mut dyn Write, bytes: &[u8]| {
        out.write_all(bytes)
            .map_err(|e| Refusal::os("sending staged files", e))
    };
    let mut counts = Counts::default();
    let mut learned = Vec::new();
    for item in items {
        match &item.source {
            Source::Bytes(bytes) => {
                let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                let entry = Entry {
                    path: item.name.clone(),
                    mode: 0o644,
                    what: What::File { size },
                    meta: None,
                };
                counts.add(size, limits)?;
                send(out, &files::head(&entry, limits).map_err(refused)?)?;
                send(out, bytes)?;
            }
            Source::Path(path) => walk(out, &item.name, path, limits, &mut counts)?,
            Source::Tar(path) => {
                tarball(
                    out,
                    &item.name,
                    &[(path.clone(), Hash::No, false)],
                    limits,
                    &mut counts,
                    true,
                )?;
            }
            Source::Layers(layers) => {
                let layers: Vec<(PathBuf, Hash, bool)> = layers
                    .iter()
                    .zip(present.iter().copied().chain(std::iter::repeat(false)))
                    .map(|(l, held)| {
                        let hash = match l.digest {
                            Some(d) if admitting => Hash::Verify(d),
                            Some(_) => Hash::No,
                            None => Hash::Learn,
                        };
                        (l.path.clone(), hash, held)
                    })
                    .collect();
                learned.extend(tarball(
                    out,
                    &item.name,
                    &layers,
                    limits,
                    &mut counts,
                    false,
                )?);
            }
        }
    }
    Ok(learned)
}

/// What the packer does with a tarball's bytes as it reads them: nothing (a root of one copy), check them
/// against the layer's digest, or learn its digest (a layer of none known).
#[derive(Debug, Clone, Copy)]
enum Hash {
    No,
    Verify(files::Digest),
    Learn,
}

/// Entries and bytes sent so far, refused past the stage's bounds before they are sent.
#[derive(Default)]
struct Counts {
    entries: u64,
    bytes: u64,
}

impl Counts {
    fn add(&mut self, size: u64, limits: &Limits) -> Result<(), Refusal> {
        let over = |what: &'static str, bound: u64| Refusal::Exhausted {
            what,
            bound: usize::try_from(bound).unwrap_or(usize::MAX),
        };
        self.entries = self
            .entries
            .checked_add(1)
            .filter(|n| *n <= limits.entries)
            .ok_or_else(|| over("entries in one stage", limits.entries))?;
        self.bytes = self
            .bytes
            .checked_add(size)
            .filter(|n| *n <= limits.bytes)
            .ok_or_else(|| over("file and xattr bytes in one stage", limits.bytes))?;
        Ok(())
    }
}

/// Sends `path` as `name`, and everything under it.
fn walk(
    out: &mut dyn Write,
    name: &str,
    path: &Path,
    limits: &Limits,
    counts: &mut Counts,
) -> Result<(), Refusal> {
    // Directories found and not yet listed: their host path and staged name. A directory's entries
    // are sent after it, so the relay makes each parent first.
    let mut dirs: Vec<(PathBuf, String)> = Vec::new();
    one(out, name, path, limits, counts, &mut dirs)?;
    while let Some((dir, dir_name)) = dirs.pop() {
        let listing = fs::read_dir(&dir).map_err(|e| {
            Refusal::os(
                "listing a folder to stage",
                format!("{}: {e}", dir.display()),
            )
        })?;
        for entry in listing {
            let entry = entry.map_err(|e| {
                Refusal::os(
                    "listing a folder to stage",
                    format!("{}: {e}", dir.display()),
                )
            })?;
            let file_name = entry.file_name();
            let child = entry.path();
            let file_name = file_name
                .to_str()
                .ok_or_else(|| never(&child, "its name isn't UTF-8"))?;
            let child_name = format!("{dir_name}/{file_name}");
            one(out, &child_name, &child, limits, counts, &mut dirs)?;
        }
    }
    Ok(())
}

/// Sends one entry: a directory's listing is left to the walk.
fn one(
    out: &mut dyn Write,
    name: &str,
    path: &Path,
    limits: &Limits,
    counts: &mut Counts,
    dirs: &mut Vec<(PathBuf, String)>,
) -> Result<(), Refusal> {
    let meta = fs::symlink_metadata(path).map_err(|e| {
        Refusal::os(
            "reading a path to stage",
            format!("{}: {e}", path.display()),
        )
    })?;
    let mode = mode(&meta);
    let (what, size) = match kind(path, &meta)? {
        Kind::Dir => (What::Dir, 0),
        Kind::File(size) => (What::File { size }, size),
        Kind::Symlink => {
            let target = fs::read_link(path).map_err(|e| {
                Refusal::os(
                    "reading a link to stage",
                    format!("{}: {e}", path.display()),
                )
            })?;
            let target = target
                .to_str()
                .ok_or_else(|| never(path, "its target isn't UTF-8"))?
                .to_owned();
            (What::Symlink { target }, 0)
        }
    };
    counts.add(size, limits)?;
    // A host directory holds no Linux owners, times or xattrs to carry (§4a).
    let entry = Entry {
        path: name.to_owned(),
        mode,
        what,
        meta: None,
    };
    let head = files::head(&entry, limits).map_err(refused)?;
    out.write_all(&head)
        .map_err(|e| Refusal::os("sending staged files", e))?;
    match entry.what {
        What::Dir => dirs.push((path.to_owned(), name.to_owned())),
        What::File { size } if size > 0 => copy(out, path, size)?,
        What::File { .. }
        | What::Symlink { .. }
        | What::Hardlink { .. }
        | What::Layer
        | What::Whiteout
        | What::Opaque => {}
    }
    Ok(())
}

/// Sends exactly `size` bytes of the file at `path`: one that changed size while it was sent is refused,
/// as its stream would no longer say what it holds.
fn copy(out: &mut dyn Write, path: &Path, size: u64) -> Result<(), Refusal> {
    let file = File::open(path).map_err(|e| {
        Refusal::os(
            "opening a file to stage",
            format!("{}: {e}", path.display()),
        )
    })?;
    let sent = io::copy(&mut file.take(size), out)
        .map_err(|e| Refusal::os("sending a staged file", format!("{}: {e}", path.display())))?;
    if sent != size {
        return Err(never(path, "it shrank while it was staged"));
    }
    Ok(())
}

/// A tar block: an archive is a sequence of 512-octet blocks, its end two blocks of zeros (POSIX.1-2024,
/// pax, "ustar Interchange Format").
const TAR_BLOCK: u64 = 512;
/// The most bytes read past a tarball's end-of-archive blocks, to reach a gzip stream's trailer and check
/// it: an archive is padded to its record, whose size POSIX's pax supports up to 32,256 bytes (POSIX.1-2024,
/// pax, `-b blocksize`: "all blocksize values less than or equal to 32256 that are multiples of 512"); one
/// longer is refused rather than read without end (CLAUDE.md §2).
const TRAILING: u64 = 32_256;

/// An extension record's whole text, at most `bound` bytes; `None` past it.
fn record(entry: &mut dyn Read, bound: usize) -> io::Result<Option<Vec<u8>>> {
    let mut text = Vec::new();
    entry
        .take(u64::try_from(bound).unwrap_or(u64::MAX).saturating_add(1))
        .read_to_end(&mut text)?;
    if text.len() > bound {
        return Ok(None);
    }
    Ok(Some(text))
}

/// A path's or link target's longest text: Linux's `PATH_MAX` (include/uapi/linux/limits.h, 4096 bytes)
/// less its NUL.
const PATH_TEXT_MAX: usize = 4096 - 1;
/// A uid's or gid's longest text: an id is a `u32` (chown(2)), `u32::MAX` = 4294967295, 10 digits.
const ID_TEXT_MAX: usize = 10;
/// A time's longest text: `i64::MIN` seconds, "-9223372036854775808" (20), a `.`, and nanoseconds, 9
/// digits (utimensat(2)'s `tv_nsec` is under 10^9; a finer fraction is refused).
const TIME_TEXT_MAX: usize = 20 + 1 + 9;
/// A user or group name's longest: glibc's `LOGIN_NAME_MAX` (sysdeps/unix/sysv/linux/bits/local_lim.h, 256).
const LOGIN_NAME_MAX: usize = 256;
/// A global `comment`'s longest: `git archive` writes its commit's id there, at most 64 hex digits
/// (SHA-256 object names, git's Documentation/technical/hash-function-transition.adoc).
const COMMENT_MAX: usize = 64;
/// A `charset` or `hdrcharset` value's longest: "ISO-IR 10646 2000 UTF-8", 23 bytes, the longest the POSIX
/// pax format's charset table names (POSIX.1-2024, pax, "pax Extended Header Keyword Descriptions").
const CHARSET_MAX: usize = 23;
/// The xattr keys' prefixes: star's (value raw) and libarchive's (name percent-encoded, value base64).
const SCHILY_XATTR: &str = "SCHILY.xattr.";
const LIBARCHIVE_XATTR: &str = "LIBARCHIVE.xattr.";
/// A base64 value's longest: an xattr value's, `XATTR_SIZE_MAX`, in 4 characters per 3 bytes (RFC 4648
/// §4): 4 × ⌈65,536 / 3⌉ = 87,384.
const BASE64_SIZE_MAX: usize = files::XATTR_SIZE_MAX.div_ceil(3) * 4;
/// The longest key staging reads: libarchive's xattr prefix and a name of `XATTR_NAME_MAX` bytes, each
/// percent-encoded as `%XX`: 17 + 3 × 255 = 782.
const KEY_MAX: usize = LIBARCHIVE_XATTR.len() + 3 * files::XATTR_NAME_MAX;

/// The decimal digits of `n`.
fn digits(n: usize) -> usize {
    let mut d: usize = 1;
    let mut rest = n;
    while rest >= 10 {
        rest = rest.checked_div(10).unwrap_or(0);
        d = d.saturating_add(1);
    }
    d
}

/// The longest PAX line, `<len> <key>=<value>\n`, of a key of `key` bytes and a value of at most `value`:
/// `<len>` counts the line, its own digits included.
fn line_max(key: usize, value: usize) -> usize {
    let body = key.saturating_add(value).saturating_add(3);
    body.saturating_add(digits(body.saturating_add(digits(body))))
}

/// The longest line staging reads (libarchive's longest xattr), whose `<len>` bounds every line's digits.
fn any_line_max() -> usize {
    line_max(KEY_MAX, BASE64_SIZE_MAX)
}

/// The longest value staging accepts for `key` in a record, or why it refuses the key outright.
fn value_max(key: &[u8], global: bool) -> Result<usize, String> {
    let named = |key: &[u8]| String::from_utf8_lossy(key).into_owned();
    if key.starts_with(b"GNU.sparse.") {
        return Err(
            "a sparse entry is never staged (none of the roots it serves holds one, §4a)"
                .to_owned(),
        );
    }
    if global {
        // What staging doesn't carry and so can't get wrong (atime, which Go's archive/tar leaves
        // unapplied for containerd, returning the record as an entry of its own; ctime, the kernel's;
        // owner names, which numeric ids stand for; a comment; the character sets the record's texts are
        // in). Any other key would apply to every entry after it, and is refused rather than ignored
        // (CLAUDE.md §3; docs/design/shards-guest.md §4a): `uid`, `gid` and `mtime` among them, since
        // staging carries those.
        return match key {
            b"atime" | b"ctime" => Ok(TIME_TEXT_MAX),
            b"uname" | b"gname" => Ok(LOGIN_NAME_MAX),
            b"comment" => Ok(COMMENT_MAX),
            b"charset" | b"hdrcharset" => Ok(CHARSET_MAX),
            other => Err(format!(
                "a global PAX record sets {}, which staging doesn't apply",
                named(other)
            )),
        };
    }
    match key {
        b"path" | b"linkpath" => Ok(PATH_TEXT_MAX),
        b"uid" | b"gid" => Ok(ID_TEXT_MAX),
        b"mtime" | b"atime" => Ok(TIME_TEXT_MAX),
        b"size" => Err("a PAX size (a file past 8 GiB) is never staged".to_owned()),
        key if key.starts_with(SCHILY_XATTR.as_bytes()) => {
            if key.len().saturating_sub(SCHILY_XATTR.len()) > files::XATTR_NAME_MAX {
                return Err(format!(
                    "an xattr's name is past Linux's XATTR_NAME_MAX, {}",
                    files::XATTR_NAME_MAX
                ));
            }
            Ok(files::XATTR_SIZE_MAX)
        }
        key if key.starts_with(LIBARCHIVE_XATTR.as_bytes()) => Ok(BASE64_SIZE_MAX),
        other => Err(format!(
            "a PAX record sets {}, which staging doesn't apply",
            named(other)
        )),
    }
}

/// What an entry's PAX record says of it: its name and link target past what a header holds, its owner
/// and times past what a header holds, and its extended attributes.
#[derive(Debug, Default)]
struct Pax {
    path: Option<Vec<u8>>,
    link: Option<Vec<u8>>,
    uid: Option<u32>,
    gid: Option<u32>,
    mtime: Option<Mtime>,
    atime: Option<Mtime>,
    xattrs: Vec<Xattr>,
    /// The longest line read, the most any one read held (non-vacuity for the bound, in tests).
    peak: usize,
}

/// One byte of `entry`, or `None` at its end.
fn byte(entry: &mut dyn Read) -> io::Result<Option<u8>> {
    let mut b = [0u8; 1];
    loop {
        match entry.read(&mut b) {
            Ok(0) => return Ok(None),
            Ok(_) => {
                let [v] = b;
                return Ok(Some(v));
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// Reads a PAX record (`global` or an entry's) from `entry` a line at a time, never holding more than one
/// line, each bounded by its key's longest before any of its value is read; a key staging doesn't apply,
/// one set twice, and an xattr past Linux's bounds are refused by name. An entry's record says what
/// [`Pax`] holds; a global one sets nothing staging carries.
fn pax_record(
    entry: &mut dyn Read,
    global: bool,
    bad: &dyn Fn(&str, &dyn std::fmt::Display) -> Refusal,
) -> Result<Pax, Refusal> {
    let refuse = |why: &dyn std::fmt::Display| bad("a PAX record", why);
    let io = |e: io::Error| bad("a PAX record", &e);
    let mut pax = Pax::default();
    let mut seen: BTreeSet<Vec<u8>> = BTreeSet::new();
    // Every xattr name, each with its NUL, within Linux's XATTR_LIST_MAX, carried or not.
    let mut names: usize = 0;
    let len_digits = digits(any_line_max());
    loop {
        // `<len> `: at most the digits of the longest line.
        let mut len: usize = 0;
        let mut taken: usize = 0;
        loop {
            match byte(entry).map_err(io)? {
                None if taken == 0 => return Ok(pax),
                None => return Err(refuse(&"it ends inside a line")),
                Some(b' ') if taken > 0 => break,
                Some(d @ b'0'..=b'9') if taken < len_digits => {
                    len = len
                        .saturating_mul(10)
                        .saturating_add(usize::from(d.saturating_sub(b'0')));
                    taken = taken.saturating_add(1);
                }
                Some(_) => return Err(refuse(&"a line's length isn't one")),
            }
        }
        // `<key>=`: at most the longest key staging reads.
        let mut key: Vec<u8> = Vec::new();
        loop {
            match byte(entry).map_err(io)? {
                None => return Err(refuse(&"it ends inside a line")),
                Some(b'=') => break,
                Some(_) if key.len() >= KEY_MAX => {
                    return Err(refuse(&format!(
                        "a key starting {:?} is longer than any staging reads",
                        String::from_utf8_lossy(&key)
                    )));
                }
                Some(b) => key.push(b),
            }
        }
        let shown = String::from_utf8_lossy(&key).into_owned();
        let most = value_max(&key, global).map_err(|why| refuse(&why))?;
        let bound = line_max(key.len(), most);
        if len > bound {
            return Err(refuse(&format!(
                "its {shown} line of {len} bytes is past the {bound} its key allows"
            )));
        }
        if !seen.insert(key.clone()) {
            return Err(refuse(&format!("it sets {shown} twice")));
        }
        // What's left of the line: its value and its newline.
        let head = taken
            .saturating_add(1)
            .saturating_add(key.len())
            .saturating_add(1);
        let rest = len.checked_sub(head).filter(|n| *n >= 1).ok_or_else(|| {
            refuse(&format!(
                "its {shown} line's length is shorter than the line"
            ))
        })?;
        let mut value = vec![0u8; rest];
        entry.read_exact(&mut value).map_err(io)?;
        pax.peak = pax.peak.max(len);
        if value.pop() != Some(b'\n') {
            return Err(refuse(&format!(
                "its {shown} line doesn't end where its length says"
            )));
        }
        if global {
            continue;
        }
        let number = |what: &str, value: &[u8]| {
            std::str::from_utf8(value)
                .ok()
                .filter(|v| v.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|v| v.parse::<u32>().ok())
                .ok_or_else(|| refuse(&format!("its {what} isn't an id")))
        };
        match key.as_slice() {
            b"path" => pax.path = Some(value),
            b"linkpath" => pax.link = Some(value),
            b"uid" => pax.uid = Some(number("uid", &value)?),
            b"gid" => pax.gid = Some(number("gid", &value)?),
            b"mtime" => {
                pax.mtime =
                    Some(pax_time(&value).ok_or_else(|| refuse(&"its mtime isn't a time"))?);
            }
            b"atime" => {
                pax.atime =
                    Some(pax_time(&value).ok_or_else(|| refuse(&"its atime isn't a time"))?);
            }
            key => {
                let (name, value) = if let Some(name) = key.strip_prefix(SCHILY_XATTR.as_bytes()) {
                    (name.to_vec(), value)
                } else if let Some(name) = key.strip_prefix(LIBARCHIVE_XATTR.as_bytes()) {
                    let name = percent_decoded(name)
                        .ok_or_else(|| refuse(&"an xattr's name isn't percent-encoded"))?;
                    let value = base64_decoded(&value)
                        .ok_or_else(|| refuse(&"an xattr's value isn't base64"))?;
                    (name, value)
                } else {
                    return Err(refuse(&format!(
                        "a PAX record sets {shown}, which staging doesn't apply"
                    )));
                };
                if name.is_empty()
                    || name.len() > files::XATTR_NAME_MAX
                    || value.len() > files::XATTR_SIZE_MAX
                {
                    return Err(refuse(&format!(
                        "xattr {:?} is past Linux's XATTR_NAME_MAX or XATTR_SIZE_MAX",
                        String::from_utf8_lossy(&name)
                    )));
                }
                names = names.saturating_add(name.len()).saturating_add(1);
                if names > files::XATTR_LIST_MAX {
                    return Err(refuse(&format!(
                        "its xattrs' names together are past Linux's XATTR_LIST_MAX, {}",
                        files::XATTR_LIST_MAX
                    )));
                }
                let name =
                    String::from_utf8(name).map_err(|_| refuse(&"an xattr's name isn't UTF-8"))?;
                // One in no namespace Linux holds (`com.apple.*`, from macOS bsdtar) can't exist on a
                // Linux file: left out, which loses nothing Linux's own unpack would hold (§4a).
                if files::XATTR_NAMESPACES
                    .iter()
                    .any(|ns| name.starts_with(ns))
                {
                    pax.xattrs.push(Xattr { name, value });
                }
            }
        }
    }
}

/// A PAX time, `seconds[.fraction]`, possibly negative.
fn pax_time(value: &[u8]) -> Option<Mtime> {
    let text = std::str::from_utf8(value).ok()?;
    let (secs, frac) = text.split_once('.').unwrap_or((text, ""));
    let secs: i64 = secs.parse().ok()?;
    // Nanoseconds at most: utimensat(2) takes no finer a time.
    if frac.len() > 9 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let digits = frac;
    let nanos: u32 = if digits.is_empty() {
        0
    } else {
        let scale = 10u32.checked_pow(u32::try_from(9_usize.checked_sub(digits.len())?).ok()?)?;
        digits.parse::<u32>().ok()?.checked_mul(scale)?
    };
    Some(Mtime { secs, nanos })
}

/// `%XX`-decoded bytes (libarchive's encoding of an xattr's name), or `None` when malformed.
fn percent_decoded(text: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len());
    let mut rest = text;
    while let Some((&b, after)) = rest.split_first() {
        if b == b'%' {
            let hex = after.get(..2)?;
            let hex = std::str::from_utf8(hex).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            rest = after.get(2..)?;
        } else {
            out.push(b);
            rest = after;
        }
    }
    Some(out)
}

/// Standard base64 (RFC 4648 §4), padding optional, or `None` when malformed.
fn base64_decoded(text: &[u8]) -> Option<Vec<u8>> {
    let value = |c: u8| -> Option<u32> {
        Some(u32::from(match c {
            b'A'..=b'Z' => c.checked_sub(b'A')?,
            b'a'..=b'z' => c.checked_sub(b'a')?.checked_add(26)?,
            b'0'..=b'9' => c.checked_sub(b'0')?.checked_add(52)?,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        }))
    };
    let text: Vec<u8> = text.iter().copied().filter(|c| *c != b'=').collect();
    let mut out = Vec::with_capacity(text.len().saturating_mul(3) / 4);
    for chunk in text.chunks(4) {
        let mut acc: u32 = 0;
        for &c in chunk {
            acc = (acc << 6) | value(c)?;
        }
        let bits = chunk.len().checked_mul(6)?;
        acc <<= 24usize.checked_sub(bits)?;
        let bytes = acc.to_be_bytes();
        let n = match chunk.len() {
            4 => 3,
            3 => 2,
            2 => 1,
            _ => return None,
        };
        out.extend_from_slice(bytes.get(1..1usize.checked_add(n)?)?);
    }
    Some(out)
}

/// A tarball's bytes, counted as they're read, so its end is checked against its last entry's.
struct Counted<R> {
    inner: R,
    read: u64,
}

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read = self
            .read
            .saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
        Ok(n)
    }
}

/// OCI's whiteout prefix: `.wh.<name>` in a layer removes `<name>` from the layers below (image-spec
/// `layer.md`, "Whiteouts"; containerd `pkg/archive/tar.go:123`).
const WHITEOUT: &str = ".wh.";
/// OCI's opaque marker: what its directory holds from the layers below is removed (`tar.go:131`).
const OPAQUE: &str = ".wh..wh..opq";

/// What the packer knows of a root as its layers apply, by the names its entries give (never resolved
/// through a link: the relay resolves within the root, §4a). It refuses early what the relay would, a
/// hard link to nothing of the root; a name through one of the root's own links is the relay's to
/// resolve.
#[derive(Default)]
struct Tree {
    dirs: BTreeSet<String>,
    links: BTreeSet<String>,
    regular: BTreeSet<String>,
    /// What the current layer has made: what an opaque marker keeps (containerd's `unpackedPaths`).
    made: BTreeSet<String>,
}

/// `rel`'s proper prefixes, nearest the root first: `a`, `a/b` for `a/b/c`.
fn prefixes(rel: &str) -> impl Iterator<Item = &str> {
    rel.match_indices('/').filter_map(|(i, _)| rel.get(..i))
}

/// The names in `set` under `dir` (`dir/…`), or every name when `dir` is the root's `""`.
fn under<'s>(set: &'s BTreeSet<String>, dir: &str) -> Vec<&'s String> {
    if dir.is_empty() {
        return set.iter().collect();
    }
    let from = format!("{dir}/");
    set.range(from.clone()..)
        .take_while(|k| k.starts_with(&from))
        .collect()
}

impl Tree {
    /// Whether `rel` is reached through one of the root's own symbolic links.
    fn through_link(&self, rel: &str) -> bool {
        prefixes(rel).any(|p| self.links.contains(p))
    }

    /// Forgets `rel` and everything under it: removed by a whiteout, or replaced.
    fn forget(&mut self, rel: &str) {
        for set in [&mut self.dirs, &mut self.links, &mut self.regular] {
            set.remove(rel);
            let gone: Vec<String> = under(set, rel).into_iter().cloned().collect();
            for k in gone {
                set.remove(&k);
            }
        }
    }

    /// Forgets what `dir` holds that this layer didn't make, with everything under it, as containerd's
    /// walk removes it (`tar.go:184-216`).
    fn forget_unmade(&mut self, dir: &str) {
        let skip = if dir.is_empty() {
            0
        } else {
            dir.len().saturating_add(1)
        };
        let made = &self.made;
        // Kept only when it and each directory between it and `dir` were made by this layer.
        let kept = |k: &str| {
            made.contains(k)
                && prefixes(k)
                    .filter(|p| p.len() >= skip)
                    .all(|p| made.contains(p))
        };
        for set in [&mut self.dirs, &mut self.links, &mut self.regular] {
            let gone: Vec<String> = under(set, dir)
                .into_iter()
                .filter(|k| !kept(k))
                .cloned()
                .collect();
            for k in gone {
                set.remove(&k);
            }
        }
    }
}

/// Sends the tarball root at `layers` (each with whether the guest holds it already) as `name`: each
/// layer's entries after a layer marker, applied over those before it by containerd's overlay rules, each
/// counted against the stage's bounds before it is sent, and every fault refused naming the archive and
/// the entry. A layer the guest holds is sent no more. A root applied `in_place` (a bundle's or a
/// `Rootfs::Dir`'s one tarball, which its container writes into) is a directory `name` the relay makes
/// first; a layered root's layers go to the guest's store, under no directory of the box's.
fn tarball(
    out: &mut dyn Write,
    name: &str,
    layers: &[(PathBuf, Hash, bool)],
    limits: &Limits,
    counts: &mut Counts,
    in_place: bool,
) -> Result<Vec<Learned>, Refusal> {
    if layers.len() > MAX_LAYERS {
        return Err(Refusal::Invalid {
            what: "staged files",
            why: format!(
                "{name} has {} layers, over overlayfs's {MAX_LAYERS}",
                layers.len()
            ),
        });
    }
    if in_place {
        // The root itself, 0755 and the relay's: an archive's own `./` entry, if it has one, names
        // nothing more (the root is the box's mount point, whose owner and times are the runtime's).
        counts.add(0, limits)?;
        let root = Entry {
            path: name.to_owned(),
            mode: 0o755,
            what: What::Dir,
            meta: None,
        };
        out.write_all(&files::head(&root, limits).map_err(refused)?)
            .map_err(|e| Refusal::os("sending staged files", e))?;
    }
    let mut tree = Tree::default();
    let mut learned = Vec::new();
    for (path, hash, held) in layers {
        if *held {
            continue;
        }
        counts.add(0, limits)?;
        let marker = Entry {
            path: name.to_owned(),
            mode: 0,
            what: What::Layer,
            meta: None,
        };
        out.write_all(&files::head(&marker, limits).map_err(refused)?)
            .map_err(|e| Refusal::os("sending staged files", e))?;
        tree.made = BTreeSet::new();
        learned.extend(layer(out, name, path, *hash, limits, counts, &mut tree)?);
    }
    Ok(learned)
}

/// The later of two times: containerd sets atime so (`latestTime`, `tar.go:335,445`).
fn later(a: Mtime, b: Mtime) -> Mtime {
    if (a.secs, a.nanos) > (b.secs, b.nanos) {
        a
    } else {
        b
    }
}

/// Sends one layer's entries, the archive at `path`, under `name`: gzip or uncompressed, as its own bytes'
/// magic says (containerd's `compression.DetectCompression` reads them so). Its bytes are hashed as they are
/// read when `hash` says: checked against its digest, the layer refused, before anything after it is sent,
/// unless they are that digest (the relay keeps a layer only once the next one's marker or the stage's end
/// follows it); or learned, returned with the file's identity when that held still while it was read.
fn layer(
    out: &mut dyn Write,
    name: &str,
    path: &Path,
    hash: Hash,
    limits: &Limits,
    counts: &mut Counts,
    tree: &mut Tree,
) -> Result<Option<Learned>, Refusal> {
    let bad = |what: &str, why: &dyn std::fmt::Display| never(path, &format!("{what}: {why}"));
    let send = |out: &mut dyn Write, entry: &Entry| {
        out.write_all(&files::head(entry, limits).map_err(refused)?)
            .map_err(|e| Refusal::os("sending staged files", e))
    };
    let file = File::open(path).map_err(|e| bad("opening it", &e))?;
    let opened = file.metadata().map_err(|e| bad("opening it", &e))?;
    // The most bytes past what the archive needs that its digest covers: the file's own length.
    let length = opened.len();
    let mut raw = io::BufReader::new(Hashing {
        file,
        hash: match hash {
            Hash::No => None,
            Hash::Verify(_) | Hash::Learn => {
                Some(Hasher::start().map_err(|e| bad("starting its hash", &e))?)
            }
        },
    });
    let head = raw.fill_buf().map_err(|e| bad("reading it", &e))?;
    let (gzip, zstd) = (head.starts_with(&GZIP_MAGIC), head.starts_with(&ZSTD_MAGIC));
    if zstd {
        return Err(bad(
            "its compression",
            &"zstd, which the packer doesn't read; give the layer gzip or uncompressed",
        ));
    }
    let reader = if gzip {
        Unzipped::Gzip(flate2::read::GzDecoder::new(raw))
    } else {
        Unzipped::Plain(raw)
    };
    let mut archive = tar::Archive::new(Counted {
        inner: reader,
        read: 0,
    });
    // Where the last entry's data ends: the archive must go on past it to its end-of-archive block.
    let mut end: u64 = 0;
    // The extension records before an entry (GNU long name and link, PAX), each read within its bound:
    // the crate's own reading of them holds a record whole, however long (tar 0.4.46 `archive.rs:418-440`),
    // so it reads raw entries and Hex reads the records.
    let mut long_name: Option<Vec<u8>> = None;
    let mut long_link: Option<Vec<u8>> = None;
    let mut pax: Option<Pax> = None;
    let entries = archive
        .entries()
        .map_err(|e| bad("reading it", &e))?
        .raw(true);
    for entry in entries {
        let mut entry = entry.map_err(|e| bad("reading an entry", &e))?;
        let kind = entry.header().entry_type();
        let size = entry.size();
        end = entry
            .raw_file_position()
            .checked_add(size.div_ceil(TAR_BLOCK).saturating_mul(TAR_BLOCK))
            .ok_or_else(|| bad("an entry", &"its size is past any archive's"))?;
        match kind {
            tar::EntryType::GNULongName | tar::EntryType::GNULongLink => {
                let slot = if kind == tar::EntryType::GNULongName {
                    &mut long_name
                } else {
                    &mut long_link
                };
                if slot.is_some() {
                    return Err(bad("a long-name record", &"two describe one entry"));
                }
                // Its text and a NUL, at most the path bound.
                let mut text = record(&mut entry, limits.path.saturating_add(1))
                    .map_err(|e| bad("a long-name record", &e))?
                    .ok_or_else(|| bad("a long-name record", &"its name is past the path bound"))?;
                while text.last() == Some(&0) {
                    text.pop();
                }
                *slot = Some(text);
                continue;
            }
            tar::EntryType::XHeader | tar::EntryType::XGlobalHeader => {
                if kind == tar::EntryType::XHeader {
                    if pax.is_some() {
                        return Err(bad("a PAX record", &"two describe one entry"));
                    }
                    pax = Some(pax_record(&mut entry, false, &bad)?);
                } else {
                    pax_record(&mut entry, true, &bad)?;
                }
                continue;
            }
            _ => {}
        }
        let mut ext = pax.take().unwrap_or_default();
        let raw = match long_name.take().or(ext.path.take()) {
            Some(name) => name,
            None => entry.header().path_bytes().into_owned(),
        };
        let link = long_link.take().or(ext.link.take()).or_else(|| {
            entry
                .header()
                .link_name_bytes()
                .map(std::borrow::Cow::into_owned)
        });
        if raw.len() > limits.path {
            return Err(bad("an entry", &"its name is past the path bound"));
        }
        let shown = String::from_utf8_lossy(&raw).into_owned();
        let text = std::str::from_utf8(&raw).map_err(|_| bad(&shown, &"its name isn't UTF-8"))?;
        // A literal absolute or `..` name is refused, where containerd clamps it to the root (§4a); a
        // name through one of the root's own links is resolved within it by the relay.
        if text.starts_with('/') {
            return Err(bad(&shown, &"its name is absolute"));
        }
        let mut rel = text.trim_end_matches('/');
        while let Some(rest) = rel.strip_prefix("./") {
            rel = rest;
        }
        if rel.split('/').any(|part| part == "..") {
            return Err(bad(&shown, &"its name has a '..' component"));
        }
        if rel.is_empty() || rel == "." {
            continue;
        }
        let (parent, leaf) = rel.rsplit_once('/').unwrap_or(("", rel));
        // A parent no layer made, the relay makes 0755 as containerd's `mkparent` does (`tar.go:448-503`).
        if !tree.through_link(rel) {
            for p in prefixes(rel) {
                if !tree.dirs.contains(p) {
                    tree.dirs.insert(p.to_owned());
                }
            }
        }
        let under_root = |rel: &str| {
            if rel.is_empty() {
                name.to_owned()
            } else {
                format!("{name}/{rel}")
            }
        };
        // A whiteout or opaque marker, whatever its kind in the archive, as containerd reads it
        // (`tar.go:180-225`): never a file of its own.
        if let Some(gone) = leaf.strip_prefix(WHITEOUT) {
            counts.add(0, limits)?;
            let (what, at) = if leaf == OPAQUE {
                tree.forget_unmade(parent);
                (What::Opaque, parent.to_owned())
            } else {
                if gone.is_empty() {
                    return Err(bad(&shown, &"a whiteout that names nothing"));
                }
                let at = if parent.is_empty() {
                    gone.to_owned()
                } else {
                    format!("{parent}/{gone}")
                };
                tree.forget(&at);
                (What::Whiteout, at)
            };
            send(
                out,
                &Entry {
                    path: under_root(&at),
                    mode: 0,
                    what,
                    meta: None,
                },
            )?;
            continue;
        }
        let header = entry.header();
        let mode = permissions(header.mode().map_err(|e| bad(&shown, &e))?);
        // A numeric field of NULs and spaces alone reads as 0, as GNU tar and Python's tarfile read it
        // (tar-rs refuses it); anything else that isn't a number is refused by name.
        let numeric = |value: io::Result<u64>, field: &[u8]| match value {
            Ok(v) => Some(v),
            Err(_) if field.iter().all(|b| *b == 0 || *b == b' ') => Some(0),
            Err(_) => None,
        };
        let id = |value: io::Result<u64>, field: &[u8], what: &str| {
            numeric(value, field)
                .and_then(|v| u32::try_from(v).ok())
                .ok_or_else(|| bad(&shown, &format!("its {what} isn't an id")))
        };
        let mtime = match ext.mtime {
            Some(mtime) => mtime,
            None => Mtime {
                secs: numeric(header.mtime(), &header.as_old().mtime)
                    .and_then(|t| i64::try_from(t).ok())
                    .ok_or_else(|| bad(&shown, &"its mtime isn't a time"))?,
                nanos: 0,
            },
        };
        // An archive's atime is PAX `atime`, or a GNU header's own field when it isn't zero, as Go's
        // archive/tar reads them for containerd; absent, it's the mtime.
        let atime = match ext.atime {
            Some(atime) => atime,
            None => header
                .as_gnu()
                .filter(|gnu| gnu.atime.first().is_some_and(|b| *b != 0))
                .and_then(|gnu| numeric(gnu.atime(), &gnu.atime))
                .and_then(|t| i64::try_from(t).ok())
                .map_or(mtime, |secs| Mtime { secs, nanos: 0 }),
        };
        let meta = Meta {
            uid: match ext.uid {
                Some(uid) => uid,
                None => id(header.uid(), &header.as_old().uid, "uid")?,
            },
            gid: match ext.gid {
                Some(gid) => gid,
                None => id(header.gid(), &header.as_old().gid, "gid")?,
            },
            mtime,
            atime: later(atime, mtime),
            xattrs: std::mem::take(&mut ext.xattrs),
        };
        let mut hardlinks_a_link = false;
        let what = match header.entry_type() {
            tar::EntryType::Directory => What::Dir,
            tar::EntryType::Regular | tar::EntryType::Continuous => What::File { size },
            tar::EntryType::Symlink => {
                let target = link
                    .clone()
                    .ok_or_else(|| bad(&shown, &"a symbolic link with no target"))?;
                if target.len() > limits.path {
                    return Err(bad(&shown, &"its target is past the path bound"));
                }
                let target = String::from_utf8(target)
                    .map_err(|_| bad(&shown, &"its target isn't UTF-8"))?;
                What::Symlink { target }
            }
            tar::EntryType::GNUSparse => {
                return Err(bad(
                    &shown,
                    &"a sparse entry is never staged (none of the roots it serves holds one, §4a)",
                ));
            }
            tar::EntryType::Char | tar::EntryType::Block => {
                return Err(bad(&shown, &"a device node is never staged"));
            }
            tar::EntryType::Fifo => return Err(bad(&shown, &"a FIFO is never staged")),
            tar::EntryType::Link => {
                // Another name for a regular file or symbolic link this layer made (linkat(2) follows
                // no link, as containerd's `link` doesn't, `tar.go:386-394`; overlay's unpack links
                // within the layer's own directory, so an earlier layer's file fails, ENOENT), named as
                // an entry is: never an escaping name, a later or missing entry, or a directory. One
                // named through a link is the relay's to resolve, which refuses it there likewise.
                let target = link
                    .clone()
                    .ok_or_else(|| bad(&shown, &"a hard link with no target"))?;
                let target = String::from_utf8(target)
                    .map_err(|_| bad(&shown, &"its target isn't UTF-8"))?;
                let mut to = target.trim_end_matches('/');
                while let Some(rest) = to.strip_prefix("./") {
                    to = rest;
                }
                if to.starts_with('/') || to.split('/').any(|part| part == "..") {
                    return Err(bad(
                        &shown,
                        &format!("a hard link names {target:?}, which names its way out"),
                    ));
                }
                let named = tree.regular.contains(to) || tree.links.contains(to);
                if named && !tree.made.contains(to) && !tree.through_link(to) {
                    return Err(bad(
                        &shown,
                        &format!(
                            "a hard link names {target:?}, a file an earlier layer made, which \
                             containerd's overlay unpack can't link (ENOENT in this layer)"
                        ),
                    ));
                }
                if !named && !tree.through_link(to) {
                    return Err(bad(
                        &shown,
                        &format!(
                            "a hard link names {target:?}, which is no earlier file or link this layer made"
                        ),
                    ));
                }
                hardlinks_a_link = tree.links.contains(to);
                What::Hardlink {
                    target: under_root(to),
                }
            }
            _ => return Err(bad(&shown, &"an entry of a kind staging carries not")),
        };
        // A hard link shares its target's inode, owner, mode, xattrs and times: it carries none of its own.
        let meta = match what {
            What::Hardlink { .. } => None,
            _ => Some(meta),
        };
        // A file's bytes and its xattrs' spend the stage's byte budget, as the relay counts them.
        let size = match what {
            What::File { size } => size,
            _ => 0,
        };
        counts.add(
            size.saturating_add(meta.as_ref().map_or(0, files::xattr_bytes)),
            limits,
        )?;
        let staged = Entry {
            path: under_root(rel),
            mode,
            what,
            meta,
        };
        send(out, &staged)?;
        // It replaces whatever was at its name, but a directory onto a directory merges (`tar.go:304-311`).
        let merges = matches!(staged.what, What::Dir) && tree.dirs.contains(rel);
        if !merges {
            tree.forget(rel);
        }
        tree.made.insert(rel.to_owned());
        match staged.what {
            What::Dir => {
                tree.dirs.insert(rel.to_owned());
            }
            What::Symlink { .. } => {
                tree.links.insert(rel.to_owned());
            }
            What::File { size } => {
                tree.regular.insert(rel.to_owned());
                if size > 0 {
                    let sent =
                        io::copy(&mut (&mut entry).take(size), out).map_err(|e| bad(&shown, &e))?;
                    if sent != size {
                        return Err(bad(&shown, &"the archive ends inside it"));
                    }
                }
            }
            What::Hardlink { .. } if hardlinks_a_link => {
                tree.links.insert(rel.to_owned());
            }
            What::Hardlink { .. } => {
                tree.regular.insert(rel.to_owned());
            }
            What::Layer | What::Whiteout | What::Opaque => {}
        }
    }
    if long_name.is_some() || long_link.is_some() || pax.is_some() {
        return Err(bad("its end", &"an extension record has no entry after it"));
    }
    let mut counted = archive.into_inner();
    if counted.read < end.saturating_add(TAR_BLOCK) {
        return Err(bad("its end", &"it ends without its end-of-archive block"));
    }
    // Read to the stream's end, so a gzip trailer that lies about the archive is caught.
    let trailing = io::copy(
        &mut (&mut counted).take(TRAILING.saturating_add(1)),
        &mut io::sink(),
    )
    .map_err(|e| bad("its end", &e))?;
    if trailing > TRAILING {
        return Err(bad(
            "its end",
            &format!("more than {TRAILING} bytes follow its end-of-archive block"),
        ));
    }
    if matches!(hash, Hash::No) {
        return Ok(None);
    }
    // The rest of the file, past the stream the archive needed: its digest covers every byte.
    let mut raw = counted.inner.into_raw();
    io::copy(&mut (&mut raw).take(length), &mut io::sink()).map_err(|e| bad("its end", &e))?;
    let Hashing { file, hash: hashed } = raw.into_inner();
    let Some(hashed) = hashed else {
        return Err(bad("its digest", &"its bytes weren't hashed"));
    };
    let got = hashed.finish().map_err(|e| bad("its digest", &e))?;
    match hash {
        Hash::Verify(want) if got != want => Err(bad(
            "its digest",
            &format!(
                "its bytes' sha256 is {}, not {}, the digest it is named by",
                files::digest_hex(&got),
                files::digest_hex(&want)
            ),
        )),
        Hash::Learn => {
            // Learned only if the file held still while it was read: a write between would make the
            // digest some other bytes'.
            let read = file.metadata().map_err(|e| bad("its end", &e))?;
            if identity(&opened) != identity(&read) {
                return Ok(None);
            }
            Ok(Some(Learned {
                path: path.to_owned(),
                digest: got,
                identity: identity(&opened),
            }))
        }
        Hash::Verify(_) | Hash::No => Ok(None),
    }
}

/// gzip's magic (RFC 1952 §2.3.1, ID1 and ID2).
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
/// A zstd frame's magic, 0xFD2FB528 little-endian (RFC 8878 §3.1.1).
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

/// A layer's file, its bytes hashed as they are read when its digest is to be checked.
struct Hashing {
    file: File,
    hash: Option<Hasher>,
}

impl Read for Hashing {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.file.read(buf)?;
        if let Some(hash) = self.hash.as_ref() {
            hash.feed(buf.get(..n).unwrap_or_default())?;
        }
        Ok(n)
    }
}

/// A sha256 taken on a thread of its own from the very bytes the packer reads, so hashing a layer overlaps
/// reading, unzipping and sending it rather than adding to them: hashed in line, it was about 0.8 s of a
/// grafana create that missed the store (480 MB at sha2 0.10.9's 606 MB/s on a Mac Studio, its software path),
/// which made a miss slower than whole-root staging (HEX-GUEST-22, 4x: 5007 ms against 4274). The channel
/// holds one chunk: one is hashed while the next is read; more depth buys nothing once the slower side is
/// busy, and a full channel makes the reader wait, so what's in flight is bounded.
struct Hasher {
    chunks: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
    done: std::thread::JoinHandle<files::Digest>,
}

impl Hasher {
    fn start() -> io::Result<Self> {
        let (chunks, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(1);
        let done = std::thread::Builder::new()
            .name("hex-layer-hash".to_owned())
            .spawn(move || {
                let mut hash = <sha2::Sha256 as sha2::Digest>::new();
                for chunk in rx {
                    sha2::Digest::update(&mut hash, &chunk);
                }
                sha2::Digest::finalize(hash).into()
            })?;
        Ok(Self {
            chunks: Some(chunks),
            done,
        })
    }

    fn feed(&self, bytes: &[u8]) -> io::Result<()> {
        let Some(chunks) = self.chunks.as_ref() else {
            return Err(io::Error::other("the layer's hash was already taken"));
        };
        chunks
            .send(bytes.to_vec())
            .map_err(|_| io::Error::other("the layer's hashing thread ended"))
    }

    /// The digest of every byte fed, once the thread has hashed the last.
    fn finish(mut self) -> io::Result<files::Digest> {
        drop(self.chunks.take());
        self.done
            .join()
            .map_err(|_| io::Error::other("the layer's hashing thread failed"))
    }
}

/// A layer's archive: gzip's stream, or the file's own bytes.
enum Unzipped {
    Gzip(flate2::read::GzDecoder<io::BufReader<Hashing>>),
    Plain(io::BufReader<Hashing>),
}

impl Read for Unzipped {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Unzipped::Gzip(gz) => gz.read(buf),
            Unzipped::Plain(raw) => raw.read(buf),
        }
    }
}

impl Unzipped {
    /// The file under it, read as far as the archive needed (what its buffer holds is hashed already).
    fn into_raw(self) -> io::BufReader<Hashing> {
        match self {
            Unzipped::Gzip(gz) => gz.into_inner(),
            Unzipped::Plain(raw) => raw,
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod tar_tests;
