//! The server's profiles (`docs/observe.md` §8.11). Each upload's profiles are kept as they came
//! (gzip'd, zstd'd or in LZ4's frames; one sent as it was, LZ4'd) in its segment's blob file
//! (`<dir>/<created ms>.kprof`), and its header (its times, its profiler's tags, each profile's
//! name, sample types, values summed, place, length and CRC-32C) in the segment's header file
//! (`.khead`), framed by its length and CRC-32C. Headers are indexed in memory (a series of tags and
//! a set of profiles interned: 40 bytes an upload), read again from the header files alone as the
//! server starts; a file's torn tail (a crash's) ends its reading, and each start writes a segment
//! of its own, so none is written past a tail. A segment is written for an hour, or 256 MiB, at
//! the most; whole segments are let go past the retention (8 days, Datadog's flame graphs'), past
//! the store's bytes, or while the disk is short. Kept on the server alone, not replicated: a
//! server's disk lost, its profiles are.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::hash::Hash;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::ops::{ControlFlow, Range};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError, RwLock};

use super::intake::{Encoding, MAX_PROFILE, Upload};

/// Profiles kept this long (Datadog keeps flame graphs eight days).
pub const RETENTION_MS: i64 = 8 * 86_400_000;
/// A segment written this long, or its files this many bytes, at the most.
const SEGMENT_MS: i64 = 3_600_000;
const SEGMENT_BYTES: u64 = 256 << 20;
/// A header's bytes at the most; a string's in one (a tag, a name, an endpoint), its tags, its
/// profiles and a profile's sample types, and its endpoints.
const MAX_HEADER: usize = 1 << 20;
const MAX_STR: usize = 512;
const MAX_TAGS: usize = 128;
const MAX_PROFILES: usize = 32;
const MAX_TYPES: usize = 32;
const MAX_ENDPOINTS: usize = 10_000;
/// Segment files' first bytes.
const BLOBS_MAGIC: &[u8; 8] = b"KPROFB01";
const HEADS_MAGIC: &[u8; 8] = b"KPROFH01";
/// Tags left out of an upload's series: they change every upload.
const VOLATILE: &[&str] = &["profile_seq"];
/// Segments a reader keeps open at once.
const READER_FILES: usize = 64;

/// How a profile's bytes are kept: as they came (gzip, zstd, LZ4's frames), or, sent as they were,
/// in LZ4's block format, or as they were where that is no smaller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kept {
    Plain = 0,
    Gzip = 1,
    Zstd = 2,
    Lz4Frames = 3,
    Lz4Block = 4,
}

impl Kept {
    fn of(b: u8) -> Option<Self> {
        Some(match b {
            0 => Self::Plain,
            1 => Self::Gzip,
            2 => Self::Zstd,
            3 => Self::Lz4Frames,
            4 => Self::Lz4Block,
            _ => return None,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("an upload's header past {MAX_HEADER} bytes")]
    TooBig,
    #[error("a record that does not read ({0})")]
    Corrupt(&'static str),
}

/// Where an upload's header is: its segment, its frame's place and length in the header file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Handle {
    pub segment: i64,
    pub head: u32,
    pub head_len: u32,
}

/// An upload as indexed. Its series and its set of profiles are interned.
#[derive(Debug, Clone, Copy)]
struct Entry {
    start_ns: i64,
    end_ns: i64,
    series: u32,
    shape: u32,
    segment: i64,
    head: u32,
    head_len: u32,
}

/// A series: its profiler's language and its tags, `(key, value)` strings' ids, sorted.
type SeriesKey = (u32, Box<[(u32, u32)]>);
/// A set of profiles: each its name and its sample types, `(type, unit)` strings' ids.
type Shape = Box<[(u32, Box<[(u32, u32)]>)]>;

/// Values interned: each one's id its place.
struct Interned<K> {
    all: Vec<K>,
    of: HashMap<K, u32, foldhash::fast::RandomState>,
}

impl<K: Clone + Hash + Eq> Default for Interned<K> {
    fn default() -> Self {
        Self { all: Vec::new(), of: HashMap::default() }
    }
}

impl<K: Clone + Hash + Eq> Interned<K> {
    fn id(&mut self, k: K) -> u32 {
        if let Some(&i) = self.of.get(&k) {
            return i;
        }
        let i = u32::try_from(self.all.len()).unwrap_or(u32::MAX);
        self.all.push(k.clone());
        self.of.insert(k, i);
        i
    }

    fn get(&self, i: u32) -> Option<&K> {
        self.all.get(i as usize)
    }
}

#[derive(Default)]
struct Strings {
    all: Vec<Box<str>>,
    of: HashMap<Box<str>, u32, foldhash::fast::RandomState>,
}

impl Strings {
    fn id(&mut self, s: &str) -> u32 {
        if let Some(&i) = self.of.get(s) {
            return i;
        }
        let i = u32::try_from(self.all.len()).unwrap_or(u32::MAX);
        self.all.push(s.into());
        self.of.insert(s.into(), i);
        i
    }

    fn get(&self, i: u32) -> &str {
        self.all.get(i as usize).map_or("", |s| s)
    }

    fn find(&self, s: &str) -> Option<u32> {
        self.of.get(s).copied()
    }
}

/// A segment: when it was made (its name), its files' bytes, its newest upload's arrival.
#[derive(Debug, Clone, Copy)]
struct Segment {
    created: i64,
    bytes: u64,
    newest_ms: i64,
}

#[derive(Default)]
struct Index {
    /// By their start.
    entries: Vec<Entry>,
    /// By their making.
    segments: Vec<Segment>,
    strings: Strings,
    series: Interned<SeriesKey>,
    shapes: Interned<Shape>,
}

impl Index {
    fn bytes(&self) -> u64 {
        self.segments.iter().map(|s| s.bytes).sum()
    }

    fn segment(&mut self, created: i64) -> Option<&mut Segment> {
        let i = self.segments.binary_search_by_key(&created, |s| s.created).ok()?;
        self.segments.get_mut(i)
    }

    fn tags_of(&self, series: u32) -> &[(u32, u32)] {
        self.series.get(series).map_or(&[], |(_, t)| t.as_ref())
    }

    fn shape_of(&self, shape: u32) -> &[(u32, Box<[(u32, u32)]>)] {
        self.shapes.get(shape).map_or(&[], AsRef::as_ref)
    }

    /// Indexes the upload `h` holds.
    fn add(&mut self, h: &HeadRef<'_>, at: Handle) {
        let mut tags: Vec<(u32, u32)> = h.tags.iter().filter(|(k, _)| !VOLATILE.contains(k)).map(|(k, v)| (self.strings.id(k), self.strings.id(v))).collect();
        tags.sort_unstable();
        tags.dedup();
        let family = self.strings.id(h.family);
        let series = self.series.id((family, tags.into_boxed_slice()));
        let shape: Shape = h
            .profiles
            .iter()
            .map(|p| {
                let types: Box<[(u32, u32)]> = h.types.get(p.types.clone()).unwrap_or_default().iter().map(|(t, u, _)| (self.strings.id(t), self.strings.id(u))).collect();
                (self.strings.id(p.name), types)
            })
            .collect();
        let shape = self.shapes.id(shape);
        let e = Entry { start_ns: h.start_ns, end_ns: h.end_ns, series, shape, segment: at.segment, head: at.head, head_len: at.head_len };
        let i = self.entries.partition_point(|x| x.start_ns <= e.start_ns);
        self.entries.insert(i, e);
    }

    /// Its strings, series and sets of profiles made again from the entries left: those of
    /// segments let go no longer held.
    fn compact(&mut self) {
        let (mut strings, mut series, mut shapes) = (Strings::default(), Interned::<SeriesKey>::default(), Interned::<Shape>::default());
        let (mut series_map, mut shape_map): (HashMap<u32, u32>, HashMap<u32, u32>) = (HashMap::new(), HashMap::new());
        let (old, old_series, old_shapes) = (&self.strings, &self.series, &self.shapes);
        for e in &mut self.entries {
            let s = *series_map.entry(e.series).or_insert_with(|| {
                let (family, tags) = old_series.get(e.series).cloned().unwrap_or_default();
                let tags: Box<[(u32, u32)]> = tags.iter().map(|&(k, v)| (strings.id(old.get(k)), strings.id(old.get(v)))).collect();
                let family = strings.id(old.get(family));
                series.id((family, tags))
            });
            let p = *shape_map.entry(e.shape).or_insert_with(|| {
                let shape: Shape = old_shapes
                    .get(e.shape)
                    .map(|s| s.iter().map(|(n, ts)| (strings.id(old.get(*n)), ts.iter().map(|&(t, u)| (strings.id(old.get(t)), strings.id(old.get(u)))).collect())).collect())
                    .unwrap_or_default();
                shapes.id(shape)
            });
            (e.series, e.shape) = (s, p);
        }
        (self.strings, self.series, self.shapes) = (strings, series, shapes);
    }
}

fn read_lock<T>(l: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(PoisonError::into_inner)
}

fn write_lock<T>(l: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(PoisonError::into_inner)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `s` cut to `n` bytes at the most, at a character's edge.
pub(super) fn clip(s: &str, n: usize) -> &str {
    let mut i = n.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    s.get(..i).unwrap_or_default()
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    let s = clip(s, MAX_STR);
    out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

/// Bytes read in place.
struct Bytes<'a> {
    at: &'a [u8],
}

impl<'a> Bytes<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], StoreError> {
        let (b, rest) = self.at.split_at_checked(n).ok_or(StoreError::Corrupt("cut short"))?;
        self.at = rest;
        Ok(b)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], StoreError> {
        self.take(N)?.try_into().map_err(|_| StoreError::Corrupt("cut short"))
    }
    fn u8(&mut self) -> Result<u8, StoreError> {
        let [b] = self.array::<1>()?;
        Ok(b)
    }
    fn u16(&mut self) -> Result<u16, StoreError> {
        Ok(u16::from_le_bytes(self.array()?))
    }
    fn u32(&mut self) -> Result<u32, StoreError> {
        Ok(u32::from_le_bytes(self.array()?))
    }
    fn u64(&mut self) -> Result<u64, StoreError> {
        Ok(u64::from_le_bytes(self.array()?))
    }
    fn i64(&mut self) -> Result<i64, StoreError> {
        Ok(i64::from_le_bytes(self.array()?))
    }
    fn str(&mut self) -> Result<&'a str, StoreError> {
        let n = usize::from(self.u16()?);
        std::str::from_utf8(self.take(n)?).map_err(|_| StoreError::Corrupt("text"))
    }
}

/// A profile in a header: its name, how it is kept, its length inflated, its place, length and
/// CRC-32C in the blob file, its sample types (a range of the header's).
#[derive(Debug, Clone)]
struct ProfRef<'a> {
    name: &'a str,
    kept: Kept,
    plain_len: u32,
    offset: u64,
    len: u32,
    crc: u32,
    types: Range<usize>,
}

/// A header read in place.
#[derive(Debug, Default)]
struct HeadRef<'a> {
    received_ms: i64,
    start_ns: i64,
    end_ns: i64,
    family: &'a str,
    tags: Vec<(&'a str, &'a str)>,
    profiles: Vec<ProfRef<'a>>,
    /// Each profile's sample types: type, unit, values summed.
    types: Vec<(&'a str, &'a str, i64)>,
    /// Its endpoints' counts in the blob file: place, length, CRC-32C.
    endpoints: (u64, u32, u32),
    /// Its blobs' end in the blob file.
    blobs_end: u64,
}

const HEAD_VERSION: u8 = 1;

impl<'a> HeadRef<'a> {
    /// `b` read into `self` (its vectors kept, emptied first).
    fn read(&mut self, b: &'a [u8]) -> Result<(), StoreError> {
        let mut r = Bytes { at: b };
        if r.u8()? != HEAD_VERSION {
            return Err(StoreError::Corrupt("its version"));
        }
        (self.received_ms, self.start_ns, self.end_ns) = (r.i64()?, r.i64()?, r.i64()?);
        self.family = r.str()?;
        self.tags.clear();
        for _ in 0..r.u16()? {
            self.tags.push((r.str()?, r.str()?));
        }
        self.profiles.clear();
        self.types.clear();
        self.blobs_end = 0;
        for _ in 0..r.u8()? {
            let name = r.str()?;
            let kept = Kept::of(r.u8()?).ok_or(StoreError::Corrupt("how a profile is kept"))?;
            let (plain_len, offset, len, crc) = (r.u32()?, r.u64()?, r.u32()?, r.u32()?);
            let from = self.types.len();
            for _ in 0..r.u8()? {
                self.types.push((r.str()?, r.str()?, r.i64()?));
            }
            self.blobs_end = self.blobs_end.max(offset.saturating_add(u64::from(len)));
            self.profiles.push(ProfRef { name, kept, plain_len, offset, len, crc, types: from..self.types.len() });
        }
        self.endpoints = (r.u64()?, r.u32()?, r.u32()?);
        self.blobs_end = self.blobs_end.max(self.endpoints.0.saturating_add(u64::from(self.endpoints.1)));
        Ok(())
    }
}

/// A segment's files.
fn blobs_path(dir: &Path, created: i64) -> PathBuf {
    dir.join(format!("{created}.kprof"))
}

fn heads_path(dir: &Path, created: i64) -> PathBuf {
    dir.join(format!("{created}.khead"))
}

/// The segment being written.
struct Open {
    created: i64,
    blobs: File,
    heads: File,
    blobs_len: u64,
    heads_len: u64,
}

#[derive(Default)]
struct Writer {
    open: Option<Open>,
    blobs: Vec<u8>,
    heads: Vec<u8>,
    head: Vec<u8>,
}

/// What [`Store::put`] kept, and refused (a header past its bound).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Put {
    pub kept: usize,
    pub refused: usize,
}

/// What [`Store::retain`] let go.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Retained {
    pub segments: usize,
    pub uploads: usize,
    pub bytes: u64,
}

/// The store's size now.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub uploads: usize,
    pub segments: usize,
    pub bytes: u64,
    pub series: usize,
    /// Its oldest and newest upload's start.
    pub oldest_ns: Option<i64>,
    pub newest_ns: Option<i64>,
}

/// The store: its folder, its writer, its index.
pub struct Store {
    dir: PathBuf,
    writer: Mutex<Writer>,
    index: RwLock<Index>,
    retention_ms: AtomicI64,
    max_bytes: AtomicU64,
}

impl Store {
    /// The store in `dir` (made where it is not), its header files read.
    pub fn open(dir: &Path) -> Result<Self, StoreError> {
        std::fs::create_dir_all(dir)?;
        let mut made: Vec<i64> = std::fs::read_dir(dir)?
            .filter_map(Result::ok)
            .filter_map(|e| {
                let name = e.file_name();
                let name = name.to_str()?;
                let stem = name.strip_suffix(".kprof").or_else(|| name.strip_suffix(".khead"))?;
                stem.parse::<i64>().ok()
            })
            .collect();
        made.sort_unstable();
        made.dedup();
        let mut ix = Index::default();
        let mut buf = Vec::new();
        for created in made {
            let size = |p: PathBuf| std::fs::metadata(p).map_or(0, |m| m.len());
            let (blob_bytes, head_bytes) = (size(blobs_path(dir, created)), size(heads_path(dir, created)));
            ix.segments.push(Segment { created, bytes: blob_bytes + head_bytes, newest_ms: created });
            buf.clear();
            if File::open(heads_path(dir, created)).and_then(|mut f| f.read_to_end(&mut buf)).is_err() || !buf.starts_with(HEADS_MAGIC) {
                continue;
            }
            let (mut at, mut newest) = (HEADS_MAGIC.len(), created);
            let mut head = HeadRef::default();
            while let Some((payload, next)) = frame(&buf, at) {
                // Its blobs past its blob file's end (that file lost, or cut): not indexed.
                if head.read(payload).is_ok() && head.blobs_end <= blob_bytes {
                    let handle = Handle { segment: created, head: u32::try_from(at).unwrap_or(u32::MAX), head_len: u32::try_from(next - at).unwrap_or(u32::MAX) };
                    ix.add(&head, handle);
                    newest = newest.max(head.received_ms);
                }
                at = next;
            }
            if at < buf.len() {
                tracing::warn!(segment = created, past = buf.len() - at, "a profile header file's tail does not read: its uploads from there on are not indexed");
            }
            if let Some(s) = ix.segment(created) {
                s.newest_ms = newest;
            }
        }
        ix.entries.sort_by_key(|e| e.start_ns);
        Ok(Self { dir: dir.to_owned(), writer: Mutex::new(Writer::default()), index: RwLock::new(ix), retention_ms: AtomicI64::new(RETENTION_MS), max_bytes: AtomicU64::new(u64::MAX) })
    }

    /// Its retention, and its bytes at the most.
    pub fn set_bounds(&self, retention_ms: i64, max_bytes: u64) {
        self.retention_ms.store(retention_ms, Ordering::Relaxed);
        self.max_bytes.store(max_bytes, Ordering::Relaxed);
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `ups`, arrived at `received_ms`, kept: their blobs written and synced, then their headers,
    /// then indexed; one whose header passes its bound refused. A write that fails is cut off where
    /// it can be, and its segment written no more.
    pub fn put(&self, ups: &[Upload], received_ms: i64) -> Result<Put, StoreError> {
        if ups.is_empty() {
            return Ok(Put::default());
        }
        let mut guard = lock(&self.writer);
        let w = &mut *guard;
        let due = w.open.as_ref().is_none_or(|o| received_ms.saturating_sub(o.created) >= SEGMENT_MS || received_ms < o.created || o.blobs_len >= SEGMENT_BYTES || o.heads_len >= SEGMENT_BYTES);
        if due {
            w.open = None;
            let open = self.create(received_ms)?;
            write_lock(&self.index).segments.push(Segment { created: open.created, bytes: open.blobs_len + open.heads_len, newest_ms: received_ms });
            w.open = Some(open);
        }
        let Some(open) = w.open.as_mut() else { return Err(StoreError::Corrupt("no segment")) };
        let (b0, h0) = (open.blobs_len, open.heads_len);
        w.blobs.clear();
        w.heads.clear();
        let mut put = Put::default();
        // Each header's handle, and its frame's place in the buffer.
        let mut framed: Vec<(Handle, usize)> = Vec::with_capacity(ups.len());
        for up in ups {
            let blobs_before = w.blobs.len();
            if encode(up, received_ms, b0, &mut w.blobs, &mut w.head).is_err() {
                w.blobs.truncate(blobs_before);
                put.refused += 1;
                continue;
            }
            let from = w.heads.len();
            w.heads.extend_from_slice(&u32::try_from(w.head.len()).unwrap_or(u32::MAX).to_le_bytes());
            w.heads.extend_from_slice(&kiln_obs::crc32c::checksum(&w.head).to_le_bytes());
            w.heads.extend_from_slice(&w.head);
            let handle = Handle { segment: open.created, head: u32::try_from(h0 + from as u64).unwrap_or(u32::MAX), head_len: u32::try_from(w.head.len() + 8).unwrap_or(u32::MAX) };
            framed.push((handle, from));
        }
        if framed.is_empty() {
            return Ok(put);
        }
        let wrote = append(&mut open.blobs, &w.blobs).and_then(|()| append(&mut open.heads, &w.heads));
        if let Err(e) = wrote {
            // Cut back where it can be; written no more either way.
            let _ = open.blobs.set_len(b0);
            let _ = open.heads.set_len(h0);
            w.open = None;
            return Err(e.into());
        }
        (open.blobs_len, open.heads_len) = (b0 + w.blobs.len() as u64, h0 + w.heads.len() as u64);
        let (created, size) = (open.created, open.blobs_len + open.heads_len);
        let mut ix = write_lock(&self.index);
        let mut head = HeadRef::default();
        for (handle, from) in framed {
            let payload = w.heads.get(from + 8..from + handle.head_len as usize).unwrap_or_default();
            head.read(payload)?;
            ix.add(&head, handle);
            put.kept += 1;
        }
        if let Some(s) = ix.segment(created) {
            (s.bytes, s.newest_ms) = (size, s.newest_ms.max(received_ms));
        }
        Ok(put)
    }

    /// A segment's files made, named by `at` (or the next free millisecond), the folder synced.
    fn create(&self, at: i64) -> Result<Open, StoreError> {
        let newest = read_lock(&self.index).segments.last().map_or(i64::MIN, |s| s.created);
        let mut created = at.max(newest.saturating_add(1));
        for _ in 0..1000 {
            let mut blobs = match OpenOptions::new().create_new(true).append(true).open(blobs_path(&self.dir, created)) {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    created += 1;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let mut heads = OpenOptions::new().create(true).truncate(true).write(true).open(heads_path(&self.dir, created))?;
            blobs.write_all(BLOBS_MAGIC)?;
            heads.write_all(HEADS_MAGIC)?;
            blobs.sync_all()?;
            heads.sync_all()?;
            sync_dir(&self.dir);
            return Ok(Open { created, blobs, heads, blobs_len: BLOBS_MAGIC.len() as u64, heads_len: HEADS_MAGIC.len() as u64 });
        }
        Err(StoreError::Corrupt("no free segment name"))
    }

    /// The uploads of `[from_ns, to_ns]` (by their start), newest first, each visited until
    /// `visit` breaks.
    pub fn scan(&self, from_ns: i64, to_ns: i64, visit: &mut dyn FnMut(&View<'_>) -> ControlFlow<()>) {
        let ix = read_lock(&self.index);
        let a = ix.entries.partition_point(|e| e.start_ns < from_ns);
        let b = ix.entries.partition_point(|e| e.start_ns <= to_ns);
        for e in ix.entries.get(a..b).unwrap_or_default().iter().rev() {
            if visit(&View { ix: &ix, e }).is_break() {
                return;
            }
        }
    }

    /// A reader of uploads' profiles.
    pub fn reader(&self) -> Reader<'_> {
        Reader { store: self, files: HashMap::new(), buf: Vec::new(), zstd: kiln_obs::zstd::Decoder::default() }
    }

    pub fn stats(&self) -> Stats {
        let ix = read_lock(&self.index);
        Stats {
            uploads: ix.entries.len(),
            segments: ix.segments.len(),
            bytes: ix.bytes(),
            series: ix.series.all.len(),
            oldest_ns: ix.entries.first().map(|e| e.start_ns),
            newest_ns: ix.entries.last().map(|e| e.start_ns),
        }
    }

    /// Segments let go, oldest first, and their uploads: those whose newest upload came before the
    /// retention, those past the store's bytes, and as many as free the `short` bytes the disk
    /// wants (none of the last two the segment being written).
    pub fn retain(&self, now_ms: i64, short: u64) -> Result<Retained, StoreError> {
        let mut w = lock(&self.writer);
        let mut ix = write_lock(&self.index);
        let (retention, max) = (self.retention_ms.load(Ordering::Relaxed), self.max_bytes.load(Ordering::Relaxed));
        let open = w.open.as_ref().map(|o| o.created);
        let (mut bytes, mut freed) = (ix.bytes(), 0u64);
        let mut gone: Vec<i64> = Vec::new();
        for s in &ix.segments {
            let old = s.newest_ms < now_ms.saturating_sub(retention);
            let crowded = Some(s.created) != open && (bytes > max || freed < short);
            if old || crowded {
                gone.push(s.created);
                bytes = bytes.saturating_sub(s.bytes);
                freed += s.bytes;
            }
        }
        if gone.is_empty() {
            return Ok(Retained::default());
        }
        if open.is_some_and(|o| gone.contains(&o)) {
            w.open = None;
        }
        let mut removed = Vec::with_capacity(gone.len());
        let mut failed = None;
        for &g in &gone {
            let mut ok = true;
            for p in [blobs_path(&self.dir, g), heads_path(&self.dir, g)] {
                match std::fs::remove_file(&p) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        ok = false;
                        failed = Some(e);
                    }
                }
            }
            if ok {
                removed.push(g);
            }
        }
        let before = ix.entries.len();
        ix.entries.retain(|e| !removed.contains(&e.segment));
        let freed: u64 = ix.segments.iter().filter(|s| removed.contains(&s.created)).map(|s| s.bytes).sum();
        ix.segments.retain(|s| !removed.contains(&s.created));
        let uploads = before - ix.entries.len();
        if uploads > 0 {
            ix.compact();
        }
        match failed {
            Some(e) if removed.is_empty() => Err(e.into()),
            _ => Ok(Retained { segments: removed.len(), uploads, bytes: freed }),
        }
    }
}

/// `bytes` appended to `f` and synced.
fn append(f: &mut File, bytes: &[u8]) -> std::io::Result<()> {
    f.write_all(bytes)?;
    f.sync_data()
}

/// The folder's entries made durable (a file made in it), where a folder can be synced.
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// The frame at `at` of `buf`: its payload and the next frame's place; none where it is cut short or
/// its checksum is not its payload's.
fn frame(buf: &[u8], at: usize) -> Option<(&[u8], usize)> {
    let len = u32::from_le_bytes(buf.get(at..at + 4)?.try_into().ok()?) as usize;
    let crc = u32::from_le_bytes(buf.get(at + 4..at + 8)?.try_into().ok()?);
    if len > MAX_HEADER {
        return None;
    }
    let payload = buf.get(at + 8..at + 8 + len)?;
    (kiln_obs::crc32c::checksum(payload) == crc).then_some((payload, at + 8 + len))
}

/// `up`'s blobs appended to `blobs` (the blob file's bytes from `base` on), its header written into
/// `head`.
fn encode(up: &Upload, received_ms: i64, base: u64, blobs: &mut Vec<u8>, head: &mut Vec<u8>) -> Result<(), StoreError> {
    head.clear();
    head.push(HEAD_VERSION);
    head.extend_from_slice(&received_ms.to_le_bytes());
    head.extend_from_slice(&up.start_ns.to_le_bytes());
    head.extend_from_slice(&up.end_ns.to_le_bytes());
    put_str(head, &up.family);
    let tags = up.tags.len().min(MAX_TAGS);
    head.extend_from_slice(&u16::try_from(tags).unwrap_or(0).to_le_bytes());
    for (k, v) in up.tags.iter().take(tags) {
        put_str(head, k);
        put_str(head, v);
    }
    let profiles = up.profiles.len().min(MAX_PROFILES);
    head.push(u8::try_from(profiles).unwrap_or(0));
    for a in up.profiles.iter().take(profiles) {
        let sent = up.sent(a);
        let start = blobs.len();
        let kept = match a.encoding {
            Encoding::Gzip => Kept::Gzip,
            Encoding::Zstd => Kept::Zstd,
            Encoding::Lz4 => Kept::Lz4Frames,
            Encoding::Raw => {
                kiln_obs::lz4::compress(sent, blobs);
                if blobs.len() - start < sent.len() {
                    Kept::Lz4Block
                } else {
                    blobs.truncate(start);
                    Kept::Plain
                }
            }
        };
        if kept != Kept::Lz4Block {
            blobs.extend_from_slice(sent);
        }
        let stored = blobs.get(start..).unwrap_or_default();
        put_str(head, &a.name);
        head.push(kept as u8);
        head.extend_from_slice(&u32::try_from(a.plain_len).unwrap_or(u32::MAX).to_le_bytes());
        head.extend_from_slice(&(base + start as u64).to_le_bytes());
        head.extend_from_slice(&u32::try_from(stored.len()).unwrap_or(u32::MAX).to_le_bytes());
        head.extend_from_slice(&kiln_obs::crc32c::checksum(stored).to_le_bytes());
        let types = a.types.len().min(MAX_TYPES);
        head.push(u8::try_from(types).unwrap_or(0));
        for ((t, u), total) in a.types.iter().zip(&a.totals).take(types) {
            put_str(head, t);
            put_str(head, u);
            head.extend_from_slice(&total.to_le_bytes());
        }
    }
    // Its endpoints' counts, a blob of their own: read only when asked for.
    let start = blobs.len();
    let n = up.endpoint_counts.len().min(MAX_ENDPOINTS);
    blobs.extend_from_slice(&u32::try_from(n).unwrap_or(0).to_le_bytes());
    for (e, c) in up.endpoint_counts.iter().take(n) {
        put_str(blobs, e);
        blobs.extend_from_slice(&c.to_le_bytes());
    }
    let stored = blobs.get(start..).unwrap_or_default();
    head.extend_from_slice(&(base + start as u64).to_le_bytes());
    head.extend_from_slice(&u32::try_from(stored.len()).unwrap_or(u32::MAX).to_le_bytes());
    head.extend_from_slice(&kiln_obs::crc32c::checksum(stored).to_le_bytes());
    if head.len() > MAX_HEADER {
        return Err(StoreError::TooBig);
    }
    Ok(())
}

/// An upload as indexed, while the index is read.
pub struct View<'a> {
    ix: &'a Index,
    e: &'a Entry,
}

impl<'a> View<'a> {
    pub fn start_ns(&self) -> i64 {
        self.e.start_ns
    }

    pub fn end_ns(&self) -> i64 {
        self.e.end_ns
    }

    /// Its series' id and its set of profiles' id: the same while the index is read, for a scan to
    /// remember what it decided of each.
    pub fn series(&self) -> u32 {
        self.e.series
    }

    pub fn shape(&self) -> u32 {
        self.e.shape
    }

    pub fn handle(&self) -> Handle {
        Handle { segment: self.e.segment, head: self.e.head, head_len: self.e.head_len }
    }

    pub fn family(&self) -> &'a str {
        self.ix.series.get(self.e.series).map_or("", |(f, _)| self.ix.strings.get(*f))
    }

    /// Its tags (its series': all but those that change every upload).
    pub fn tags(&self) -> impl Iterator<Item = (&'a str, &'a str)> + 'a {
        let ix = self.ix;
        ix.tags_of(self.e.series).iter().map(move |&(k, v)| (ix.strings.get(k), ix.strings.get(v)))
    }

    pub fn tag(&self, key: &str) -> Option<&'a str> {
        let k = self.ix.strings.find(key)?;
        let tags = self.ix.tags_of(self.e.series);
        let i = tags.partition_point(|&(tk, _)| tk < k);
        tags.get(i).filter(|&&(tk, _)| tk == k).map(|&(_, v)| self.ix.strings.get(v))
    }

    /// Its profiles: each one's name and sample types, `(type, unit)`.
    pub fn profiles(&self) -> impl Iterator<Item = (&'a str, Vec<(&'a str, &'a str)>)> + 'a {
        let ix = self.ix;
        ix.shape_of(self.e.shape).iter().map(move |(n, ts)| (ix.strings.get(*n), ts.iter().map(|&(t, u)| (ix.strings.get(t), ix.strings.get(u))).collect()))
    }

    /// Profile `name`'s sample type `kind` (Go's `cpu.pprof`'s `cpu`; its mutex and block profiles
    /// both have `delay`): its place among the profile's types, and its unit.
    pub fn type_of(&self, name: &str, kind: &str) -> Option<(usize, &'a str)> {
        let (n, k) = (self.ix.strings.find(name)?, self.ix.strings.find(kind)?);
        let (_, ts) = self.ix.shape_of(self.e.shape).iter().find(|(pn, _)| *pn == n)?;
        ts.iter().position(|&(t, _)| t == k).and_then(|i| Some((i, self.ix.strings.get(ts.get(i)?.1))))
    }
}

/// An upload's header as read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub received_ms: i64,
    pub start_ns: i64,
    pub end_ns: i64,
    pub family: String,
    /// All its tags, as the profiler (and its Agent) said them.
    pub tags: Vec<(String, String)>,
    /// Each profile: its name, and each sample type's (type, unit, values summed).
    pub profiles: Vec<(String, Vec<(String, String, i64)>)>,
}

/// Reads uploads' profiles: their segments' files kept open, its buffers kept, from one to the next.
pub struct Reader<'s> {
    store: &'s Store,
    files: HashMap<i64, (File, File)>,
    buf: Vec<u8>,
    zstd: kiln_obs::zstd::Decoder,
}

impl Reader<'_> {
    /// Upload `h`'s header frame into the buffer; false where its segment is gone.
    fn head(&mut self, h: Handle) -> Result<bool, StoreError> {
        if !self.files.contains_key(&h.segment) {
            if self.files.len() >= READER_FILES {
                self.files.clear();
            }
            let dir = &self.store.dir;
            match File::open(blobs_path(dir, h.segment)).and_then(|b| Ok((b, File::open(heads_path(dir, h.segment))?))) {
                Ok(f) => {
                    self.files.insert(h.segment, f);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(e) => return Err(e.into()),
            }
        }
        let Some((_, heads)) = self.files.get_mut(&h.segment) else { return Ok(false) };
        self.buf.resize(h.head_len as usize, 0);
        heads.seek(SeekFrom::Start(u64::from(h.head)))?;
        heads.read_exact(&mut self.buf)?;
        if frame(&self.buf, 0).is_none_or(|(_, next)| next != self.buf.len()) {
            return Err(StoreError::Corrupt("its header's checksum"));
        }
        Ok(true)
    }

    /// Upload `h`'s header; none where its segment is gone.
    pub fn header(&mut self, h: Handle) -> Result<Option<Header>, StoreError> {
        if !self.head(h)? {
            return Ok(None);
        }
        let mut r = HeadRef::default();
        r.read(self.buf.get(8..).unwrap_or_default())?;
        let profiles =
            r.profiles.iter().map(|p| (p.name.to_owned(), r.types.get(p.types.clone()).unwrap_or_default().iter().map(|(t, u, v)| ((*t).to_owned(), (*u).to_owned(), *v)).collect())).collect();
        let tags = r.tags.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        Ok(Some(Header { received_ms: r.received_ms, start_ns: r.start_ns, end_ns: r.end_ns, family: r.family.to_owned(), tags, profiles }))
    }

    /// Upload `h`'s endpoints' counts; none where its segment is gone.
    pub fn endpoints(&mut self, h: Handle) -> Result<Option<Vec<(String, u64)>>, StoreError> {
        if !self.head(h)? {
            return Ok(None);
        }
        let mut r = HeadRef::default();
        r.read(self.buf.get(8..).unwrap_or_default())?;
        let (offset, len, crc) = r.endpoints;
        let kept = self.blob(h.segment, offset, len, crc)?;
        let mut b = Bytes { at: &kept };
        let mut out = Vec::new();
        for _ in 0..b.u32()? {
            out.push((b.str()?.to_owned(), b.u64()?));
        }
        Ok(Some(out))
    }

    /// `len` bytes at `offset` of segment `segment`'s blob file, checked against `crc`.
    fn blob(&mut self, segment: i64, offset: u64, len: u32, crc: u32) -> Result<Vec<u8>, StoreError> {
        let Some((blobs, _)) = self.files.get_mut(&segment) else { return Err(StoreError::Corrupt("its segment")) };
        let mut kept = vec![0u8; len as usize];
        blobs.seek(SeekFrom::Start(offset))?;
        blobs.read_exact(&mut kept)?;
        if kiln_obs::crc32c::checksum(&kept) != crc {
            return Err(StoreError::Corrupt("its checksum"));
        }
        Ok(kept)
    }

    /// Upload `h`'s profile `name`, inflated into `out` (cleared first): false where it holds none
    /// or its segment is gone; its bytes checked against their CRC-32C.
    pub fn profile(&mut self, h: Handle, name: &str, out: &mut Vec<u8>) -> Result<bool, StoreError> {
        out.clear();
        if !self.head(h)? {
            return Ok(false);
        }
        let found = {
            let mut r = HeadRef::default();
            r.read(self.buf.get(8..).unwrap_or_default())?;
            r.profiles.iter().find(|p| p.name == name).map(|p| (p.kept, p.plain_len, p.offset, p.len, p.crc))
        };
        let Some((kept_as, plain_len, offset, len, crc)) = found else { return Ok(false) };
        let kept = self.blob(h.segment, offset, len, crc)?;
        let plain = (plain_len as usize).min(MAX_PROFILE);
        match kept_as {
            Kept::Plain => out.extend_from_slice(&kept),
            Kept::Gzip => {
                flate2::read::GzDecoder::new(kept.as_slice()).take(plain as u64).read_to_end(out)?;
            }
            Kept::Zstd => self.zstd.decompress(&kept, out, plain).map_err(|_| StoreError::Corrupt("its zstd"))?,
            Kept::Lz4Frames => kiln_obs::lz4::decompress_frames(&kept, out, plain).map_err(|_| StoreError::Corrupt("its LZ4 frames"))?,
            Kept::Lz4Block => kiln_obs::lz4::decompress(&kept, out, plain).map_err(|_| StoreError::Corrupt("its LZ4 block"))?,
        }
        if out.len() != plain {
            return Err(StoreError::Corrupt("its length inflated"));
        }
        Ok(true)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::super::intake::{self, tests as fx};
    use super::*;

    const HOUR_S: i64 = 1_792_771_200; // 2026-10-23T16:00:00Z, an hour's start

    fn at(s: i64) -> String {
        time::OffsetDateTime::from_unix_timestamp(s).unwrap().format(&time::format_description::well_known::Rfc3339).unwrap()
    }

    fn upload(start_s: i64, tags: &str) -> Upload {
        let body = fx::upload_of("b", &[("cpu.pprof", fx::CPU), ("delta-heap.pprof", fx::HEAP)], &at(start_s), tags);
        intake::read("multipart/form-data; boundary=b", body).unwrap()
    }

    fn all(s: &Store) -> Vec<(i64, String, Handle)> {
        let mut out = Vec::new();
        s.scan(i64::MIN, i64::MAX, &mut |v| {
            out.push((v.start_ns() / 1_000_000_000, v.tag("service").unwrap_or("").to_owned(), v.handle()));
            ControlFlow::Continue(())
        });
        out
    }

    /// Kept and listed newest first, each with its series and profiles; its profiles read back as
    /// they were inflated, its header as the profiler said it, its endpoints; the same from the
    /// files once opened again, and written on after that in a segment of its own.
    #[test]
    fn uploads_are_kept_listed_read_and_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        assert_eq!(s.put(&[upload(HOUR_S, "service:checkout,env:prod,profile_seq:1")], HOUR_S * 1000).unwrap(), Put { kept: 1, refused: 0 });
        s.put(&[upload(HOUR_S + 60, "service:search,env:prod,profile_seq:1"), upload(HOUR_S + 120, "service:checkout,env:prod,profile_seq:2")], (HOUR_S + 120) * 1000).unwrap();
        let listed = all(&s);
        assert_eq!(listed.iter().map(|(t, svc, _)| (*t - HOUR_S, svc.as_str())).collect::<Vec<_>>(), [(120, "checkout"), (60, "search"), (0, "checkout")]);
        let mut seen = Vec::new();
        s.scan(i64::MIN, i64::MAX, &mut |v| {
            let mut tags: Vec<String> = v.tags().map(|(k, v)| format!("{k}:{v}")).collect();
            tags.sort();
            seen.push((v.series(), v.family().to_owned(), tags, v.type_of("delta-heap.pprof", "alloc_space").map(|(i, u)| (i, u.to_owned())), v.profiles().count()));
            ControlFlow::Continue(())
        });
        assert_eq!(seen[0].0, seen[2].0, "one series, profile_seq left out");
        assert_ne!(seen[0].0, seen[1].0);
        assert_eq!((seen[0].1.as_str(), seen[0].2.clone(), seen[0].3.clone(), seen[0].4), ("go", vec!["env:prod".to_owned(), "service:checkout".to_owned()], Some((1, "bytes".to_owned())), 2));
        let mut r = s.reader();
        let (mut out, mut want) = (Vec::new(), Vec::new());
        intake::inflate(fx::HEAP, &mut want).unwrap();
        assert!(r.profile(listed[0].2, "delta-heap.pprof", &mut out).unwrap());
        assert_eq!(out, want, "read back as it was inflated");
        assert!(!r.profile(listed[0].2, "nonesuch.pprof", &mut out).unwrap());
        let h = r.header(listed[0].2).unwrap().unwrap();
        assert_eq!((h.received_ms, h.tags.iter().find(|(k, _)| k == "profile_seq").map(|(_, v)| v.as_str())), ((HOUR_S + 120) * 1000, Some("2")));
        assert_eq!(h.profiles[0], ("cpu.pprof".to_owned(), vec![("samples".to_owned(), "count".to_owned(), 150), ("cpu".to_owned(), "nanoseconds".to_owned(), 1_500_000_000)]));
        assert_eq!(r.endpoints(listed[0].2).unwrap().unwrap(), [("GET /cart".to_owned(), 120)]);
        drop(r);
        drop(s);
        let s = Store::open(dir.path()).unwrap();
        assert_eq!(all(&s), listed, "the same, from the files");
        s.put(&[upload(HOUR_S + 180, "service:checkout,env:prod")], (HOUR_S + 180) * 1000).unwrap();
        assert_eq!(s.stats().segments, 2, "a segment of its own once opened again");
        let s = Store::open(dir.path()).unwrap();
        assert_eq!(s.stats().uploads, 4);
        assert!(s.reader().profile(all(&s)[0].2, "cpu.pprof", &mut out).unwrap());
    }

    /// A profile sent as it was (no compression) kept LZ4'd and read back the same.
    #[test]
    fn a_profile_sent_as_it_was_is_kept_smaller_and_read_back_the_same() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let mut plain = Vec::new();
        intake::inflate(fx::HEAP, &mut plain).unwrap();
        let body = fx::upload_of("b", &[("heap.pprof", &plain)], &at(HOUR_S), "service:a");
        s.put(&[intake::read("multipart/form-data; boundary=b", body).unwrap()], HOUR_S * 1000).unwrap();
        let mut out = Vec::new();
        assert!(s.reader().profile(all(&s)[0].2, "heap.pprof", &mut out).unwrap());
        assert_eq!(out, plain);
        assert!(s.stats().bytes < plain.len() as u64, "{} bytes kept of {}", s.stats().bytes, plain.len());
    }

    /// A header file's torn tail (half a frame, a crash's) is passed over; a frame whose bytes
    /// changed ends its reading there, what came before indexed; a profile whose bytes changed is
    /// refused, not inflated wrong.
    #[test]
    fn a_torn_or_damaged_file_loses_what_is_past_the_damage_alone() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        for i in 0..3 {
            s.put(&[upload(HOUR_S + 60 * i, "service:a")], (HOUR_S + 60 * i) * 1000).unwrap();
        }
        let listed = all(&s);
        drop(s);
        let created = listed[0].2.segment;
        let heads = heads_path(dir.path(), created);
        let mut f = OpenOptions::new().append(true).open(&heads).unwrap();
        f.write_all(&[200, 0, 0, 0, 1, 2, 3, 4, 9, 9]).unwrap();
        drop(f);
        let s = Store::open(dir.path()).unwrap();
        assert_eq!(all(&s), listed, "the torn tail passed over");
        drop(s);
        let mut b = std::fs::read(&heads).unwrap();
        b[listed[1].2.head as usize + 20] ^= 0xff;
        std::fs::write(&heads, &b).unwrap();
        let s = Store::open(dir.path()).unwrap();
        assert_eq!(all(&s).len(), 1, "the second's header changed: it and the third not indexed");
        let blobs = blobs_path(dir.path(), created);
        let mut b = std::fs::read(&blobs).unwrap();
        b[BLOBS_MAGIC.len() + 100] ^= 0xff;
        std::fs::write(&blobs, &b).unwrap();
        let mut out = Vec::new();
        assert!(matches!(s.reader().profile(listed[2].2, "cpu.pprof", &mut out), Err(StoreError::Corrupt(_))));
    }

    /// Segments past the retention let go whole; past the store's bytes the oldest first, never
    /// the one being written; as many as the disk is short of; their series and strings no longer
    /// held.
    #[test]
    fn old_excess_and_crowded_segments_are_let_go() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let (day, now) = (86_400, HOUR_S);
        for d in [5, 3, 1, 0] {
            let t = now - d * day;
            s.put(&[upload(t, &format!("service:svc-{d}"))], t * 1000).unwrap();
        }
        assert_eq!(s.stats().segments, 4, "a segment an hour");
        s.set_bounds(2 * 86_400_000, u64::MAX);
        let r = s.retain(now * 1000, 0).unwrap();
        assert_eq!((r.segments, r.uploads), (2, 2), "five and three days back");
        assert_eq!(all(&s).iter().map(|(_, svc, _)| svc.as_str()).collect::<Vec<_>>(), ["svc-0", "svc-1"]);
        assert_eq!(s.stats().series, 2, "the series let go no longer held");
        let one = s.stats().bytes / 2;
        s.set_bounds(RETENTION_MS, one + 1);
        assert_eq!(s.retain(now * 1000, 0).unwrap().segments, 1, "past its bytes: the oldest");
        assert_eq!(s.retain(now * 1000, u64::MAX).unwrap().segments, 0, "the segment being written is kept");
        assert_eq!(all(&s).len(), 1);
        drop(s);
        let s = Store::open(dir.path()).unwrap();
        assert_eq!(s.retain(now * 1000, 1).unwrap().segments, 1, "opened again, none being written: the disk short, the oldest let go");
        assert_eq!((s.stats().uploads, std::fs::read_dir(dir.path()).unwrap().count()), (0, 0));
    }

    /// An upload takes 40 bytes in memory, however many tags and profiles it has.
    #[test]
    fn an_upload_takes_forty_bytes_in_memory() {
        assert_eq!(size_of::<Entry>(), 40);
    }
}
