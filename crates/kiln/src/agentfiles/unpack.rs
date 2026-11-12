//! Pinned archives unpacked into a harness's build context: the one unpacker kiln has, for every
//! built-in harness whose pinned files are archives (R12). Step 25 (Devin) wrote it, step 28 (pi) uses
//! it, and step 27 (Cursor) gave it the form steps 26 and 27 agreed (kiln plans
//! 01a1ce705e4d7008993f5c7291ef4476 and 01a1ce705e5c7504b88732f443d99288, step 2 of each; research
//! 01a1ce705e4d70089c04cee721525c8f f6, f15-f17, f26 and 01a1ce705e5c7504bdcac9721fa699e3 f10, f25).
//!
//! The context fetcher hands [`tar_gz`] a gzip tar only once the file's size and SHA-256 have matched
//! its pin, so what unpacks is what was pinned, and each entry streams from the verified file. A pin's
//! `unpack` ([`Spec`]) is one of two forms:
//! - a [`Tree`]: every entry, its first `strip` names dropped, into a staging directory beside its
//!   target, which is renamed into place at the pin file's path only once the whole archive is out,
//!   its entries and its files' bytes are the pin's, and its modes are set (Devin's bundle at strip 0;
//!   pi's and Cursor's, their leading directory stripped). The fetcher unpacks a build's trees first,
//!   then its members, then writes its harness file, so a tree's target is never there before it.
//! - a [`Member`]: one regular file, written at the pin file's path and held to its own pinned size
//!   and SHA-256 (pi's `bin/rg` and `bin/fd`; agy's binary). A member the pin says the archive holds
//!   `alone` (agy's) refuses the archive if any other entry is there.
//!
//! Only directories and regular files are taken, as Shards takes them in a harness's domain: each at a
//! relative path without `..`, with no set-user-ID, set-group-ID or sticky bit, and given 0755 when its
//! owner may execute it, else 0644. Links, devices, FIFOs and every other kind refuse the archive where
//! they'd be written; an entry a member's pick passes over isn't checked, unless the member is the
//! archive's alone. Each header is checked before anything is written, and the entries are written
//! here, never by tar's own `unpack_in`, which drops a leading `/` rather than refusing it, skips a `..`
//! path, makes links, and writes a device, a FIFO or a kind it doesn't know as a regular file (tar
//! 0.4.46 entry.rs). Nothing is overwritten, and nothing is written beside the archive's entries: an
//! installer's `distribution` marker beside Devin's `bin/` turns its self-update on (f26).

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use flate2::read::GzDecoder;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use tar::{Archive, Entry, EntryType};

/// The harness file the fetcher writes beside a tree it unpacked at the per-arch directory's root
/// (OSI's `harness.json`), which no archive may hold there.
const HARNESS_FILE: &str = "harness.json";

/// pin.json's `unpack` for a file it pins, as steps 26 and 27 agreed: a member's fields or a tree's,
/// every one of its form given and none of the other's (a [`Fields`] that isn't is refused, typed).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "Fields")]
pub enum Spec {
    Member(Member),
    Tree(Tree),
}

/// One regular file of an archive, written at the pin file's path (step 26's form).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// Its path in the archive, `.` aside.
    pub member: String,
    /// Its own SHA-256, lowercase hex.
    pub sha256: String,
    /// Its own size in bytes.
    pub size: u64,
    /// The archive holds this member and no other entry, as its pin.sh found when it pinned the archive
    /// (agy's: kiln plan 01a1ce705e55753182217afdfe607de9, step 3). Every entry is then read, and any
    /// other refuses the archive. Without it, the entries beside the member are passed over unchecked:
    /// the archives pi picks rg and fd from hold other files (step 27's contract).
    pub alone: bool,
}

/// A whole archive, its first `strip` names dropped, renamed into place at the pin file's path (step
/// 27's form). Its totals are measured from the archive at pin time, so the bound comes from the pinned
/// data (R03).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tree {
    /// How many leading names each entry's path loses, `.` not counted.
    pub strip: usize,
    /// Its regular files' bytes in all, as `tar -tv` lists their sizes.
    pub unpacked_bytes: u64,
    /// Its directories and regular files in all, as `tar -t` lists them: the leading directories the
    /// strip drops included, tar's own metadata (pax and GNU long-name headers) not.
    pub entries: u64,
}

/// pin.json's `unpack` object as written, before it's one [`Spec`]: no field beside these.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fields {
    pub format: String,
    pub member: Option<String>,
    pub sha256: Option<String>,
    pub size: Option<u64>,
    pub alone: Option<bool>,
    pub strip: Option<usize>,
    pub unpacked_bytes: Option<u64>,
    pub entries: Option<u64>,
}

impl TryFrom<Fields> for Spec {
    type Error = Why;

    fn try_from(f: Fields) -> Result<Spec, Why> {
        if f.format != "tar.gz" {
            return Err(Why::Format(f.format));
        }
        let member = f.member.is_some() || f.sha256.is_some() || f.size.is_some() || f.alone.is_some();
        let tree = f.strip.is_some() || f.unpacked_bytes.is_some() || f.entries.is_some();
        match (member, tree) {
            (true, true) => Err(Why::Both),
            (false, false) => Err(Why::Neither),
            (true, false) => Ok(Spec::Member(Member {
                member: f.member.ok_or(Why::Lacks("member"))?,
                sha256: f.sha256.ok_or(Why::Lacks("sha256"))?,
                size: f.size.ok_or(Why::Lacks("size"))?,
                alone: f.alone.unwrap_or(false),
            })),
            (false, true) => Ok(Spec::Tree(Tree {
                strip: f.strip.ok_or(Why::Lacks("strip"))?,
                unpacked_bytes: f.unpacked_bytes.ok_or(Why::Lacks("unpacked_bytes"))?,
                entries: f.entries.ok_or(Why::Lacks("entries"))?,
            })),
        }
    }
}

/// An archive that didn't unpack: the pinned URL it came from, the entry at fault (none when the fault
/// is the pin's, the directory's or the whole archive's rather than one entry's), and why.
#[derive(Debug, thiserror::Error)]
#[error("{url}{}: {why}", .entry.as_deref().map(|e| format!(": {e}")).unwrap_or_default())]
pub struct Unpack {
    pub url: String,
    pub entry: Option<String>,
    pub why: Why,
}

/// Why an archive didn't unpack.
#[derive(Debug, thiserror::Error)]
pub enum Why {
    /// A symbolic or a hard link.
    #[error("a link, which a harness may not hold")]
    Link,
    /// A character or block device, or a FIFO.
    #[error("a device or a FIFO, which a harness may not hold")]
    Device,
    /// A kind this doesn't take there, by its typeflag: neither a directory nor a regular file (tar has
    /// no kind for a socket), or a member that isn't a regular file.
    #[error("an entry of type '{}', which this doesn't take", typeflag(.0))]
    Kind(u8),
    /// A path, an entry's or the pin's, that's absolute, climbs out with `..`, or names nothing.
    #[error("a path outside the directory it unpacks into")]
    Escapes,
    /// An entry whose leading names, those the strip drops, aren't the ones the entries before it gave:
    /// another top-level directory the strip would merge in.
    #[error("outside the leading directory the strip drops")]
    Prefix,
    /// A file the strip leaves no name, which would be dropped unseen.
    #[error("a file the strip leaves no name")]
    Nameless,
    /// The same path as an entry before it, a directory's too, which one would take the other's place.
    #[error("the same path as an entry before it")]
    Duplicate,
    /// `harness.json` (ASCII case aside) at the root of a tree unpacked at the per-arch directory's,
    /// where the fetcher writes the harness's own beside it.
    #[error("harness.json at the tree's root, where kiln writes the harness's own")]
    HarnessFile,
    /// A set-user-ID or set-group-ID bit, an entry's or in the pin's `mode`.
    #[error("set-ID bits, which nothing in a harness may have")]
    SetId,
    /// The sticky bit, an entry's or in the pin's `mode`.
    #[error("the sticky bit, which nothing in a harness may have")]
    Sticky,
    /// A path that's there already: a tree's target, or a member's file (under the per-arch directory).
    #[error("{} is there already, and nothing is overwritten", .0.display())]
    Exists(PathBuf),
    /// No entry is the member the pin names.
    #[error("the archive holds no such member")]
    Missing,
    /// An entry beside the member the pin says the archive holds alone.
    #[error("an entry beside {member}, which the pin says the archive holds alone")]
    Extra { member: String },
    /// A member whose size isn't the pin's.
    #[error("{got} bytes, where the pin says {expected}")]
    Size { expected: u64, got: u64 },
    /// A member whose SHA-256 isn't the pin's.
    #[error("SHA-256 {got}, where the pin says {expected}")]
    Digest { expected: String, got: String },
    /// A tree whose regular files hold other than the pin's `unpacked_bytes`: as many as were read
    /// when it was refused (at the file that took it past the pin's, or at the end).
    #[error("{got} bytes in its files, where the pin says {expected}")]
    Bytes { expected: u64, got: u64 },
    /// A tree of other than the pin's `entries`: as many as were read when it was refused (at the entry
    /// past the pin's, or at the end).
    #[error("{got} entries, where the pin says {expected}")]
    Entries { expected: u64, got: u64 },
    /// A `format` other than `tar.gz`.
    #[error("format {0:?}: only tar.gz unpacks")]
    Format(String),
    /// A member's fields beside a tree's (`member`, `sha256`, `size` or `alone` with `strip`,
    /// `unpacked_bytes` or `entries`), which take different things.
    #[error("a member's fields and a tree's together")]
    Both,
    /// Neither a member's fields nor a tree's.
    #[error("neither a member's fields nor a tree's")]
    Neither,
    /// A field its form needs, by name.
    #[error("no {0}, which its form needs")]
    Lacks(&'static str),
    /// A `mode` for a whole tree, whose entries keep their own.
    #[error("a mode for a whole tree, whose entries keep their own")]
    TreeMode,
    #[error(transparent)]
    Io(#[from] io::Error),
}

fn typeflag(byte: &u8) -> std::ascii::EscapeDefault {
    std::ascii::escape_default(*byte)
}

/// A fault as this module finds it: the entry at fault, if one is, and why.
type Fault = (Option<String>, Why);

fn io_fault(e: io::Error) -> Fault {
    (None, Why::Io(e))
}

/// `archive`, a gzip tar whose size and SHA-256 have matched its pin, unpacked as `spec` says at the pin
/// file's `path` under `root`, the per-arch directory the fetcher is making:
/// - a tree into a staging directory beside the first directory on the way to `root/path` that isn't
///   there yet (`root` itself, for a tree at `.`), each entry checked before it's written; then that
///   directory renamed into place once the whole archive is in, its totals are the pin's and its modes
///   are set. A target there already is refused.
/// - a member as the file `path`, its parent directories made, with `mode` (the pin's), or else its
///   own as a tree's entry gets it; no entry after it is read, unless the archive holds it alone.
///
/// Nothing is ever overwritten. The archive is removed either way, and on failure so is what this made.
pub fn tar_gz(archive: &Path, url: &str, spec: &Spec, root: &Path, path: &Path, mode: Option<u32>) -> Result<(), Unpack> {
    let done = match spec {
        // The pin's own path stays inside the per-arch directory, as each entry's does.
        _ if escapes(path) => Err((None, Why::Escapes)),
        Spec::Tree(_) if mode.is_some() => Err((None, Why::TreeMode)),
        Spec::Tree(tree) => unpack_tree(archive, tree, root, &clean(path)),
        Spec::Member(member) => one(archive, member, mode, root, &clean(path)),
    };
    let done = done.and_then(|()| fs::remove_file(archive).map_err(io_fault));
    if done.is_err() {
        // Best effort: the fault returned is the first one.
        let _ = fs::remove_file(archive);
    }
    done.map_err(|(entry, why)| Unpack { url: url.to_owned(), entry, why })
}

/// The tree into staging, then staging renamed to the first directory on the way to `root/at` that
/// isn't there: so nothing of the tree shows until all of it is in. On failure staging is discarded.
fn unpack_tree(archive: &Path, tree: &Tree, root: &Path, at: &Path) -> Result<(), Fault> {
    let (unit, under) = first_missing(root, at)?;
    let staging = staging_for(&unit)?;
    fs::create_dir(&staging).map_err(io_fault)?;
    let inner = staging.join(&under);
    let done = fs::create_dir_all(&inner)
        .map_err(io_fault)
        .and_then(|()| fill(archive, tree, &inner, at))
        .and_then(|dirs| modes(&staging, &under, &dirs))
        .and_then(|()| fs::rename(&staging, &unit).map_err(io_fault));
    if done.is_err() {
        discard(&staging);
    }
    done
}

/// The first directory on the way from `root` to `root/at` that isn't there, and the rest of the way
/// under it; refused if `root/at` is there already.
fn first_missing(root: &Path, at: &Path) -> Result<(PathBuf, PathBuf), Fault> {
    let mut unit = root.to_path_buf();
    let mut rest = names(at);
    loop {
        match fs::symlink_metadata(&unit) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((unit, rest.collect())),
            Err(e) => return Err(io_fault(e)),
            Ok(_) => match rest.next() {
                Some(name) => unit.push(name),
                None => return Err((None, Why::Exists(as_shown(at)))),
            },
        }
    }
}

/// A new name beside `unit` for staging what's renamed to it: `.<name>.unpack-<UUIDv7>`.
fn staging_for(unit: &Path) -> Result<PathBuf, Fault> {
    let (Some(parent), Some(name)) = (unit.parent(), unit.file_name()) else {
        return Err((None, Why::Escapes));
    };
    Ok(parent.join(format!(".{}.unpack-{}", name.to_string_lossy(), uuid::Uuid::now_v7().simple())))
}

/// The directories a tree made under its root, each with the mode it gets, and the root's own mode, its
/// leading entry's if the archive has one.
#[derive(Default)]
struct Dirs {
    under: BTreeMap<PathBuf, u32>,
    root: Option<u32>,
}

/// Every entry of the tree into `inner` (`at`: where that is under the per-arch directory), each checked
/// before it's written, and its totals held to the pin's: as they come, so nothing past the pin's bound
/// is written, and at the end.
fn fill(archive: &Path, tree: &Tree, inner: &Path, at: &Path) -> Result<Dirs, Fault> {
    let mut tar = open(archive)?;
    let mut seen = Seen::default();
    let mut dirs = Dirs::default();
    let (mut bytes, mut entries) = (0u64, 0u64);
    for entry in tar.entries().map_err(io_fault)? {
        let mut entry = entry.map_err(io_fault)?;
        let name = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        let at_fault = |why: Why| (Some(name.clone()), why);
        let Some((path, goes)) = taken(&entry, tree.strip).map_err(at_fault)? else {
            continue;
        };
        entries = entries.saturating_add(1);
        if entries > tree.entries {
            return Err(at_fault(Why::Entries { expected: tree.entries, got: entries }));
        }
        seen.admit(&path, tree.strip).map_err(at_fault)?;
        match goes {
            Goes::Lead { mode } => dirs.root = mode.map(kept).or(dirs.root),
            Goes::File { rel, size, mode } => {
                bytes = bytes.saturating_add(size);
                if bytes > tree.unpacked_bytes {
                    return Err(at_fault(Why::Bytes { expected: tree.unpacked_bytes, got: bytes }));
                }
                harness(at, &rel).map_err(at_fault)?;
                if let Some(parent) = rel.parent() {
                    make(inner, parent, &mut dirs.under).map_err(at_fault)?;
                }
                copy(&mut entry, &inner.join(&rel), &at.join(&rel), None, kept(mode)).map_err(at_fault)?;
            }
            Goes::Dir { rel, mode } => {
                harness(at, &rel).map_err(at_fault)?;
                make(inner, &rel, &mut dirs.under).map_err(at_fault)?;
                dirs.under.insert(rel, kept(mode));
            }
        }
    }
    if entries != tree.entries {
        return Err((None, Why::Entries { expected: tree.entries, got: entries }));
    }
    if bytes != tree.unpacked_bytes {
        return Err((None, Why::Bytes { expected: tree.unpacked_bytes, got: bytes }));
    }
    Ok(dirs)
}

/// What a tree has taken so far: the leading names its strip drops, as far as its entries have shown
/// them, and every entry's path.
#[derive(Default)]
struct Seen {
    lead: Vec<OsString>,
    paths: HashSet<PathBuf>,
}

impl Seen {
    /// `path` (`.` aside) taken: refused if its leading names disagree with those before it (another
    /// top-level directory the strip would merge in), or if an entry before it had the same path.
    fn admit(&mut self, path: &Path, strip: usize) -> Result<(), Why> {
        let lead: Vec<OsString> = names(path).take(strip).map(|n| n.as_os_str().to_os_string()).collect();
        if !self.lead.iter().zip(&lead).all(|(a, b)| a == b) {
            return Err(Why::Prefix);
        }
        if lead.len() > self.lead.len() {
            self.lead = lead;
        }
        if !self.paths.insert(path.to_path_buf()) {
            return Err(Why::Duplicate);
        }
        Ok(())
    }
}

/// Where an entry of a tree goes.
enum Goes {
    /// A directory the strip leaves no name: the tree's root, with its mode, when it has `strip` names
    /// (the leading one, `dist-package/`), or one above it.
    Lead { mode: Option<u32> },
    /// A regular file of `size` bytes, to `rel` once the strip has dropped its leading names.
    File { rel: PathBuf, size: u64, mode: u32 },
    Dir { rel: PathBuf, mode: u32 },
}

/// What `entry` is: its path, `.` aside, and where it goes with its first `strip` names dropped; none
/// for tar's own metadata. Refused for a path outside the directory, for anything not [`writable`], and
/// for a file the strip would leave no name.
fn taken<R: io::Read>(entry: &Entry<'_, R>, strip: usize) -> Result<Option<(PathBuf, Goes)>, Why> {
    let kind = kind(entry);
    if meta(kind) {
        return Ok(None);
    }
    let path = entry.path()?;
    // tar would drop a root or a drive and skip a `..`: each refuses a tree, even where `strip` would
    // drop it.
    if escapes(&path) {
        return Err(Why::Escapes);
    }
    writable(entry, kind)?;
    let mode = entry.header().mode()?;
    let depth = names(&path).count();
    let goes = if depth > strip {
        let rel = names(&path).skip(strip).collect();
        if kind.is_dir() { Goes::Dir { rel, mode } } else { Goes::File { rel, size: entry.size(), mode } }
    } else if kind.is_dir() {
        Goes::Lead { mode: (depth == strip).then_some(mode) }
    } else {
        return Err(Why::Nameless);
    };
    Ok(Some((clean(&path), goes)))
}

/// Refused if `rel`, a tree's entry at `at` under the per-arch directory, is that directory's harness
/// file, ASCII case aside, as a case-insensitive file system takes names.
fn harness(at: &Path, rel: &Path) -> Result<(), Why> {
    let root = at.as_os_str().is_empty();
    if root && rel.to_str().is_some_and(|r| r.eq_ignore_ascii_case(HARNESS_FILE)) { Err(Why::HarnessFile) } else { Ok(()) }
}

/// `rel` and the directories on the way to it made under `inner`, each recorded with 0755 unless an entry
/// of its own already gave it a mode.
fn make(inner: &Path, rel: &Path, under: &mut BTreeMap<PathBuf, u32>) -> Result<(), Why> {
    fs::create_dir_all(inner.join(rel))?;
    for dir in rel.ancestors().filter(|d| !d.as_os_str().is_empty()) {
        under.entry(dir.to_path_buf()).or_insert(0o755);
    }
    Ok(())
}

/// The tree's directories given their modes, deepest first (as tar's own `Archive::unpack` orders them),
/// so each takes its own before its parent's: those under the root as recorded, the root its leading
/// entry's or else 0755, and those between staging and the root, which no entry gives one, 0755.
fn modes(staging: &Path, under: &Path, dirs: &Dirs) -> Result<(), Fault> {
    let inner = staging.join(under);
    for (rel, mode) in dirs.under.iter().rev() {
        set_dir_mode(&inner.join(rel), *mode).map_err(io_fault)?;
    }
    set_dir_mode(&inner, dirs.root.unwrap_or(0o755)).map_err(io_fault)?;
    for dir in under.ancestors().skip(1) {
        set_dir_mode(&staging.join(dir), 0o755).map_err(io_fault)?;
    }
    Ok(())
}

/// What a failed tree staged, removed (best effort: the fault returned is the first one). Each directory
/// is made its owner's to search and change first: one whose mode lacks its owner's execute bit (0644)
/// would keep `remove_dir_all` from what it holds.
fn discard(staging: &Path) {
    let mut todo = vec![staging.to_path_buf()];
    while let Some(dir) = todo.pop() {
        if set_dir_mode(&dir, 0o700).is_err() {
            continue;
        }
        if let Ok(list) = fs::read_dir(&dir) {
            todo.extend(list.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()));
        }
    }
    let _ = fs::remove_dir_all(staging);
}

/// The regular file at `member.member` in the archive, written as `root/to` with `mode`, or else its
/// own as [`kept`] gives a tree's entry its mode, and held to its pinned size and SHA-256. Every entry
/// before it is passed over unchecked and none after it read, unless the archive holds it alone: then
/// every entry is read, and any other refuses the archive (see [`pick`]). On failure nothing written for
/// it is left: neither its file nor a directory made for it.
fn one(archive: &Path, member: &Member, mode: Option<u32>, root: &Path, to: &Path) -> Result<(), Fault> {
    let path = Path::new(&member.member);
    if escapes(path) || names(path).next().is_none() || to.as_os_str().is_empty() {
        return Err((Some(member.member.clone()), Why::Escapes));
    }
    if let Some(mode) = mode {
        special(mode).map_err(|why| (Some(member.member.clone()), why))?;
    }
    let mut made = Vec::new();
    let done = pick(archive, member, mode, root, to, &mut made);
    if done.is_err() {
        // Best effort, as in `tar_gz`: the file first, then its directories, deepest first.
        for p in made.iter().rev() {
            let _ = if p.is_dir() { fs::remove_dir(p) } else { fs::remove_file(p) };
        }
    }
    done
}

/// Where [`pick`]'s pass over an archive its member is held alone in stands.
enum Pass {
    /// Neither the member nor anything beside it yet.
    Nothing,
    /// An entry beside the member, the first, and no member yet: an extra once the member comes, else
    /// the member is missing.
    Beside(String),
    /// The member, written.
    Picked,
}

/// [`one`]'s pass over the archive, listing in `made` what it writes: the member's directories as they're
/// made, shallowest first, then its file. In an archive the pin says holds the member alone, an entry
/// beside it is refused at once as what it is where that's hostile (an escaping path, a link, a device, a
/// set-ID bit), and otherwise as [`Why::Extra`] once the member is known to be in: as the member comes,
/// before a byte of it is written, for one before it; at the entry, for one after it. An archive without
/// the member is [`Why::Missing`], whatever else it holds.
fn pick(archive: &Path, member: &Member, mode: Option<u32>, root: &Path, to: &Path, made: &mut Vec<PathBuf>) -> Result<(), Fault> {
    let path = Path::new(&member.member);
    let at = |why: Why| (Some(member.member.clone()), why);
    let extra = |name: String| (Some(name), Why::Extra { member: member.member.clone() });
    let mut tar = open(archive)?;
    let mut pass = Pass::Nothing;
    for entry in tar.entries().map_err(io_fault)? {
        let mut entry = entry.map_err(io_fault)?;
        let kind = kind(&entry);
        if meta(kind) {
            continue;
        }
        if !same(&entry.path().map_err(io_fault)?, path) {
            if member.alone {
                let name = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
                hostile(&entry, kind).map_err(|why| (Some(name.clone()), why))?;
                match pass {
                    Pass::Nothing => pass = Pass::Beside(name),
                    Pass::Beside(_) => {}
                    Pass::Picked => return Err(extra(name)),
                }
            }
            continue;
        }
        match std::mem::replace(&mut pass, Pass::Nothing) {
            Pass::Nothing => {}
            Pass::Beside(name) => return Err(extra(name)),
            Pass::Picked => return Err(at(Why::Duplicate)),
        }
        if kind.is_dir() {
            return Err(at(Why::Kind(kind.as_byte())));
        }
        writable(&entry, kind).map_err(at)?;
        // Its pinned size, before a byte of it is written.
        if entry.size() != member.size {
            return Err(at(Why::Size { expected: member.size, got: entry.size() }));
        }
        let mode = match mode {
            Some(mode) => mode,
            None => kept(entry.header().mode().map_err(|e| at(Why::Io(e)))?),
        };
        parents(root, to, made).map_err(|e| at(Why::Io(e)))?;
        let file = root.join(to);
        copy(&mut entry, &file, to, Some(member.sha256.as_str()), mode).map_err(at)?;
        made.push(file);
        if !member.alone {
            return Ok(());
        }
        pass = Pass::Picked;
    }
    match pass {
        Pass::Picked => Ok(()),
        Pass::Nothing | Pass::Beside(_) => Err(at(Why::Missing)),
    }
}

/// Refused if `entry`, beside a member its archive holds alone, is hostile as a tree's entry would be:
/// an absolute or climbing path, or anything [`writable`] refuses.
fn hostile<R: io::Read>(entry: &Entry<'_, R>, kind: EntryType) -> Result<(), Why> {
    if escapes(&entry.path()?) {
        return Err(Why::Escapes);
    }
    writable(entry, kind)
}

/// `root` and `to`'s parent directories under it, made where they aren't there, with 0755; each one
/// made is listed in `made`, shallowest first.
fn parents(root: &Path, to: &Path, made: &mut Vec<PathBuf>) -> io::Result<()> {
    let mut at = root.to_path_buf();
    mkdir(&at, made)?;
    for name in to.parent().map(names).into_iter().flatten() {
        at.push(name);
        mkdir(&at, made)?;
    }
    Ok(())
}

fn mkdir(dir: &Path, made: &mut Vec<PathBuf>) -> io::Result<()> {
    match fs::create_dir(dir) {
        Ok(()) => {
            made.push(dir.to_path_buf());
            set_dir_mode(dir, 0o755)
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

/// `entry`, a regular file, streamed into a new file `to` (`shown`: its path under the per-arch
/// directory), never over what's there, held to `sha256` where one is given; then given `mode` and its
/// time as the archive has it. No part of it is left if any of that fails.
fn copy<R: io::Read>(entry: &mut Entry<'_, R>, to: &Path, shown: &Path, sha256: Option<&str>, mode: u32) -> Result<(), Why> {
    let out = match fs::OpenOptions::new().write(true).create_new(true).open(to) {
        Ok(out) => out,
        // What's there isn't this one's, and stays.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Err(Why::Exists(shown.to_path_buf())),
        Err(e) => return Err(Why::Io(e)),
    };
    let done = stream(entry, out, sha256, mode);
    if done.is_err() {
        let _ = fs::remove_file(to);
    }
    done
}

fn stream<R: io::Read>(entry: &mut Entry<'_, R>, out: fs::File, sha256: Option<&str>, mode: u32) -> Result<(), Why> {
    let size = entry.size();
    let mut out = Hashed { out, hash: sha256.map(|_| Sha256::new()) };
    if io::copy(entry, &mut out)? != size {
        return Err(Why::Io(io::ErrorKind::UnexpectedEof.into()));
    }
    if let (Some(expected), Some(hash)) = (sha256, out.hash.take()) {
        let got = hex::encode(hash.finalize());
        if got != expected {
            return Err(Why::Digest { expected: expected.to_owned(), got });
        }
    }
    set_mode(&out.out, mode)?;
    if let Some(time) = entry.header().mtime().ok().and_then(|t| UNIX_EPOCH.checked_add(Duration::from_secs(t))) {
        out.out.set_modified(time)?;
    }
    Ok(())
}

/// A file written through SHA-256, where one is asked.
struct Hashed {
    out: fs::File,
    hash: Option<Sha256>,
}

impl Write for Hashed {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.out.write(buf)?;
        if let Some(hash) = &mut self.hash {
            hash.update(buf.get(..n).unwrap_or_default());
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

/// A gzip tar, its entries read here and written by this module alone.
fn open(archive: &Path) -> Result<Archive<GzDecoder<fs::File>>, Fault> {
    Ok(Archive::new(GzDecoder::new(fs::File::open(archive).map_err(io_fault)?)))
}

/// An entry's kind as tar unpacks it: an old (non-ustar) header's regular entry whose name ends in `/`
/// is a directory (tar 0.4.46 entry.rs).
fn kind<R: io::Read>(entry: &Entry<'_, R>) -> EntryType {
    match entry.header().entry_type() {
        EntryType::Regular if entry.header().as_ustar().is_none() && entry.path_bytes().ends_with(b"/") => EntryType::Directory,
        kind => kind,
    }
}

/// tar's own metadata, no file: a pax header or a GNU long name, which tar applies to the entry each
/// precedes, or a pax global header, of which it applies nothing.
fn meta(kind: EntryType) -> bool {
    matches!(kind, EntryType::XHeader | EntryType::XGlobalHeader | EntryType::GNULongName | EntryType::GNULongLink)
}

/// Refused unless `entry` is a directory or a regular file without a set-ID or sticky bit. Its path is
/// checked apart: a tree's in `taken`, a member's as the pin names it.
fn writable<R: io::Read>(entry: &Entry<'_, R>, kind: EntryType) -> Result<(), Why> {
    match kind {
        EntryType::Regular | EntryType::Directory => {}
        EntryType::Symlink | EntryType::Link => return Err(Why::Link),
        EntryType::Char | EntryType::Block | EntryType::Fifo => return Err(Why::Device),
        other => return Err(Why::Kind(other.as_byte())),
    }
    special(entry.header().mode()?)
}

/// Refused for a set-user-ID or set-group-ID bit (`<sys/stat.h>`'s S_ISUID | S_ISGID, 0o6000) or the
/// sticky bit (S_ISVTX, 0o1000), refused rather than cleared (step 25's coordinator for set-ID, step 27
/// for both): pin.sh refuses them at pin time, as Shards' check of an artifact's content does.
fn special(mode: u32) -> Result<(), Why> {
    if mode & 0o6000 != 0 {
        return Err(Why::SetId);
    }
    if mode & 0o1000 != 0 {
        return Err(Why::Sticky);
    }
    Ok(())
}

/// The mode a tree's file or directory gets, and a member's without a pinned one: 0755 when its owner may
/// execute (a directory: search) it, S_IXUSR (0o100), else 0644, step 27's rule. Every user a harness
/// runs as may read it, and its owner alone write it, whatever the archive's other bits or the umask.
fn kept(mode: u32) -> u32 {
    if mode & 0o100 != 0 { 0o755 } else { 0o644 }
}

/// Modes are Unix's; elsewhere there's none to set.
#[cfg(unix)]
fn set_mode(file: &fs::File, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    file.set_permissions(fs::Permissions::from_mode(mode & 0o777))
}

#[cfg(not(unix))]
fn set_mode(_: &fs::File, _: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_dir_mode(dir: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(dir, fs::Permissions::from_mode(mode & 0o777))
}

#[cfg(not(unix))]
fn set_dir_mode(_: &Path, _: u32) -> io::Result<()> {
    Ok(())
}

fn escapes(path: &Path) -> bool {
    path.components().any(|c| matches!(c, Component::Prefix(_) | Component::RootDir | Component::ParentDir))
}

/// A path's names, `.` left out.
fn names(path: &Path) -> impl Iterator<Item = Component<'_>> {
    path.components().filter(|c| matches!(c, Component::Normal(_)))
}

fn clean(path: &Path) -> PathBuf {
    names(path).collect()
}

/// `at` as an error shows it: `.` for the per-arch directory itself.
fn as_shown(at: &Path) -> PathBuf {
    if at.as_os_str().is_empty() { PathBuf::from(".") } else { at.to_path_buf() }
}

/// Whether two paths in an archive are the same, `.` aside.
fn same(a: &Path, b: &Path) -> bool {
    a.components().filter(|c| *c != Component::CurDir).eq(b.components().filter(|c| *c != Component::CurDir))
}

#[cfg(test)]
mod tests;
