//! Amphora: a versioned database on disk (Amphora's `docs/design/engine.md`, decisions 6 to 8).
//!
//! The data file holds a header and the pages after it (page `n` at `n` pages in). A commit is
//! logged to its writer's lane of the redo log in timestamp order and made durable by flushes of
//! the lanes ([`Redo`]). A checkpoint pauses row changes (each holds the gate shared for its page
//! changes; the checkpoint holds it exclusive for the pause): it takes the last commit's timestamp
//! T, switches the redo log's turn at it, ends the pool's epoch of writes, and lists the rows whose
//! version in the pages isn't their version as of T: changes of transactions still open. After the
//! pause, while writers go on, it copies the pages written in that epoch as they were at the pause
//! (copy-on-update, [`Pool::copy_owed`]). The copies, the list and the new header go to the next
//! journal, flushed, then over the data file, flushed; then the redo turn switched from, which
//! holds only commits up to T, is cut. Making a database is checkpoint 0. A checkpoint that fails
//! leaves the database making nothing more durable until it is reopened: the pages it took as
//! copied may not have reached the file.
//!
//! Recovery takes the newest complete journal at or after the header's checkpoint and writes its
//! pages and header again (a power cut may keep a checkpoint's header and not every page it wrote),
//! loads the pages, merges the redo lanes by timestamp up to the first commit none holds (cutting
//! each lane after it), puts the rows of the journal's list back to their versions as of T and
//! replays the commits after T, side by side by row (SiloR), and clears the versions left on
//! cells: no snapshot outlives the process.
//!
//! Journal and header: `magic` (`u64`), the body's length (`u64`) and XXH3 (`u64`), then the body:
//! the checkpoint's sequence number (`u64`), T (`u64`), the pages handed out (`u64`), the tree count
//! (`u64`) and each tree's root (`u64`); a journal's body then holds its list's length (`u64`), its
//! list as one redo record at T, and each page's number (`u64`) and bytes to the end, written and
//! read a page at a time (G7 step 4). Journals take turns by
//! sequence number's parity, so a checkpoint never writes over the journal recovery may need.

use super::blink::{Comparator, Descender};
use super::columns::Feed;
use super::disk::{Disk, File};
use super::overflow;
use super::page::PAGE_SIZE;
use super::pool::{Pool, Room};
use super::redo::{self, LaneId, Op, Redo};
use super::version::{Savepoint, Snapshot, Ts, Txn, Versioned};
use crate::error::LimboError;
use crate::sync::{Arc, Mutex, RwLock};
use crate::Result;
use std::collections::HashMap;

const HEADER_MAGIC: u64 = 0x4144_4548_4f52_5041;
const JOURNAL_MAGIC: u64 = 0x4c4e_524a_4f52_5041;

/// A checkpoint as a header or journal names it.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Stamp {
    seq: u64,
    ts: Ts,
    allocated: u64,
    roots: Vec<u64>,
}

fn corrupt(what: &str) -> LimboError {
    LimboError::Corrupt(format!("versioned storage: {what}"))
}

fn take_u64(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let end = at.checked_add(8)?;
    let b: [u8; 8] = bytes.get(*at..end)?.try_into().ok()?;
    *at = end;
    Some(u64::from_le_bytes(b))
}

impl Stamp {
    fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        for v in [
            self.seq,
            self.ts,
            self.allocated,
            u64::try_from(self.roots.len()).map_err(|_| LimboError::IntegerOverflow)?,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        for r in &self.roots {
            out.extend_from_slice(&r.to_le_bytes());
        }
        Ok(())
    }

    fn decode(bytes: &[u8], at: &mut usize) -> Option<Self> {
        let seq = take_u64(bytes, at)?;
        let ts = take_u64(bytes, at)?;
        let allocated = take_u64(bytes, at)?;
        let n = usize::try_from(take_u64(bytes, at)?).ok()?;
        if n > bytes.len() / 8 {
            return None;
        }
        let mut roots = Vec::with_capacity(n);
        for _ in 0..n {
            roots.push(take_u64(bytes, at)?);
        }
        Some(Self {
            seq,
            ts,
            allocated,
            roots,
        })
    }
}

/// `magic`, the body's length and XXH3, the body.
fn seal(magic: u64, body: &[u8]) -> Result<Vec<u8>> {
    let len = u64::try_from(body.len()).map_err(|_| LimboError::IntegerOverflow)?;
    let mut out = Vec::with_capacity(body.len().saturating_add(24));
    out.extend_from_slice(&magic.to_le_bytes());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&twox_hash::XxHash3_64::oneshot(body).to_le_bytes());
    out.extend_from_slice(body);
    Ok(out)
}

/// The body sealed by `magic` at the front of `bytes`, if it checks out; the bytes after it (a
/// header page's padding) aren't its.
fn unseal(magic: u64, bytes: &[u8]) -> Option<&[u8]> {
    let mut at = 0;
    if take_u64(bytes, &mut at)? != magic {
        return None;
    }
    let len = usize::try_from(take_u64(bytes, &mut at)?).ok()?;
    let hash = take_u64(bytes, &mut at)?;
    let body = bytes.get(at..at.checked_add(len)?)?;
    if twox_hash::XxHash3_64::oneshot(body) == hash {
        Some(body)
    } else {
        None
    }
}

fn journal_file(seq: u64) -> File {
    if seq.is_multiple_of(2) {
        File::Journal
    } else {
        File::JournalNext
    }
}

fn page_offset(page: u64) -> Result<u64> {
    let size = u64::try_from(PAGE_SIZE).map_err(|_| LimboError::IntegerOverflow)?;
    page.checked_mul(size).ok_or(LimboError::IntegerOverflow)
}

/// The partition of the replay a row's changes go to: one per thread recovery runs.
fn partition(op: &Op, parts: usize) -> usize {
    use std::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    match op {
        Op::Put { tree, key, .. } | Op::Remove { tree, key } => {
            tree.hash(&mut h);
            key.hash(&mut h);
        }
        // Replayed alone, between the row changes ([`Database::apply`]).
        Op::Made { tree } | Op::Dropped { tree } | Op::Ordered { tree, .. } => tree.hash(&mut h),
    }
    usize::try_from(h.finish())
        .unwrap_or(usize::MAX)
        .checked_rem(parts)
        .unwrap_or(0)
}

/// A seal's own bytes ahead of its body: magic, length and hash (`seal`'s format).
const SEAL_BYTES: u64 = 24;

/// A journal page: its number (`u64`) and its bytes (the journal's format).
const ENTRY_BYTES: usize = 8 + PAGE_SIZE;

/// Where a journal's pages lie: `count` of them, `ENTRY_BYTES` each, from `at` in `file`.
struct JournalPages {
    file: File,
    at: u64,
    count: u64,
}

/// A journal's stamp and list, and where its pages lie. Its body is the stamp, the list's length
/// (`u64`) and the list (one redo record), then its pages to the end, sealed like a header. It is
/// written and read a page at a time, so a checkpoint and recovery hold one page of it in memory
/// however many it copies (G7 step 4, `NS.4`: the pool's budget bounds what a checkpoint holds).
struct Journal {
    stamp: Stamp,
    list: Vec<Op>,
    pages: JournalPages,
}

/// `len` bytes of `file` at `offset`; None if the file ends first.
fn read_exact(disk: &dyn Disk, file: File, offset: u64, len: usize) -> Result<Option<Vec<u8>>> {
    let mut buf = vec![0u8; len];
    let n = disk.read_at(file, offset, &mut buf)?;
    Ok(if n == len { Some(buf) } else { None })
}

fn to_u64(n: usize) -> Result<u64> {
    u64::try_from(n).map_err(|_| LimboError::IntegerOverflow)
}

impl Journal {
    /// Writes a journal over `stamp`'s file, flushed: the stamp, `list`, and each page `copy`
    /// hands its callback, written as it comes; the seal last, over the body's hash, so a journal
    /// cut short never checks out.
    fn write(
        disk: &dyn Disk,
        stamp: &Stamp,
        list: &[Op],
        copy: impl FnOnce(&mut dyn FnMut(u64, &[u8]) -> Result<()>) -> Result<()>,
    ) -> Result<JournalPages> {
        use std::hash::Hasher;
        let file = journal_file(stamp.seq);
        disk.truncate(file, 0)?;
        let mut head = Vec::new();
        stamp.encode(&mut head)?;
        let list = redo::encode(stamp.ts, list)?;
        head.extend_from_slice(&to_u64(list.len())?.to_le_bytes());
        head.extend_from_slice(&list);
        let mut hash = twox_hash::XxHash3_64::with_seed(0);
        hash.write(&head);
        disk.write_at(file, SEAL_BYTES, &head)?;
        let at = SEAL_BYTES
            .checked_add(to_u64(head.len())?)
            .ok_or(LimboError::IntegerOverflow)?;
        let entry_len = to_u64(ENTRY_BYTES)?;
        let (mut end, mut count) = (at, 0u64);
        let mut entry = Vec::with_capacity(ENTRY_BYTES);
        copy(&mut |no, bytes| {
            if bytes.len() != PAGE_SIZE {
                return Err(LimboError::InternalError(
                    "versioned storage: a checkpoint's page of the wrong size".into(),
                ));
            }
            entry.clear();
            entry.extend_from_slice(&no.to_le_bytes());
            entry.extend_from_slice(bytes);
            hash.write(&entry);
            disk.write_at(file, end, &entry)?;
            end = end
                .checked_add(entry_len)
                .ok_or(LimboError::IntegerOverflow)?;
            count = count.checked_add(1).ok_or(LimboError::IntegerOverflow)?;
            Ok(())
        })?;
        let len = end
            .checked_sub(SEAL_BYTES)
            .ok_or(LimboError::IntegerOverflow)?;
        let mut seal = Vec::with_capacity(24);
        for word in [JOURNAL_MAGIC, len, hash.finish()] {
            seal.extend_from_slice(&word.to_le_bytes());
        }
        disk.write_at(file, 0, &seal)?;
        disk.sync(file)?;
        Ok(JournalPages { file, at, count })
    }

    /// The journal in `file` if it is whole: its body hashed a page at a time, then its stamp and
    /// list read; its pages left in the file.
    fn read(disk: &dyn Disk, file: File) -> Result<Option<Self>> {
        use std::hash::Hasher;
        let size = disk.len(file)?;
        let Some(seal) = read_exact(disk, file, 0, 24)? else {
            return Ok(None);
        };
        let mut at = 0;
        let magic = take_u64(&seal, &mut at);
        let len = take_u64(&seal, &mut at);
        let sum = take_u64(&seal, &mut at);
        let (Some(JOURNAL_MAGIC), Some(len), Some(sum)) = (magic, len, sum) else {
            return Ok(None);
        };
        if SEAL_BYTES.checked_add(len).is_none_or(|end| end > size) {
            return Ok(None);
        }
        let mut hash = twox_hash::XxHash3_64::with_seed(0);
        let mut buf = vec![0u8; ENTRY_BYTES];
        let mut done = 0u64;
        while done < len {
            let n = usize::try_from(len.saturating_sub(done))
                .unwrap_or(usize::MAX)
                .min(ENTRY_BYTES);
            let Some(chunk) = buf.get_mut(..n) else {
                return Ok(None);
            };
            if disk.read_at(file, SEAL_BYTES.saturating_add(done), chunk)? != n {
                return Ok(None);
            }
            hash.write(chunk);
            done = done.saturating_add(to_u64(n)?);
        }
        if hash.finish() != sum {
            return Ok(None);
        }
        // The stamp's four words, the last its roots' count; then the roots and the list's length.
        let Some(words) = read_exact(disk, file, SEAL_BYTES, 32)? else {
            return Ok(None);
        };
        let mut at = 24;
        let Some(head_len) = take_u64(&words, &mut at)
            .and_then(|roots| roots.checked_mul(8))
            .and_then(|r| r.checked_add(40))
            .filter(|&h| h <= len)
        else {
            return Ok(None);
        };
        let head_len = usize::try_from(head_len).map_err(|_| LimboError::IntegerOverflow)?;
        let Some(head) = read_exact(disk, file, SEAL_BYTES, head_len)? else {
            return Ok(None);
        };
        let mut at = 0;
        let (Some(stamp), Some(list_len)) =
            (Stamp::decode(&head, &mut at), take_u64(&head, &mut at))
        else {
            return Ok(None);
        };
        let list_at = SEAL_BYTES.saturating_add(to_u64(at)?);
        let Some(pages_at) = list_at
            .checked_add(list_len)
            .filter(|&p| p <= SEAL_BYTES.saturating_add(len))
        else {
            return Ok(None);
        };
        let list_len = usize::try_from(list_len).map_err(|_| LimboError::IntegerOverflow)?;
        let Some(list) = read_exact(disk, file, list_at, list_len)? else {
            return Ok(None);
        };
        let (records, valid) = redo::decode_all(&list);
        if valid != list.len() || records.len() != 1 {
            return Ok(None);
        }
        let Some(list) = records.into_iter().next().map(|r| r.ops) else {
            return Ok(None);
        };
        let rest = SEAL_BYTES.saturating_add(len).saturating_sub(pages_at);
        let entry_len = to_u64(ENTRY_BYTES)?;
        if !rest.is_multiple_of(entry_len) {
            return Ok(None);
        }
        let count = rest.checked_div(entry_len).unwrap_or(0);
        Ok(Some(Self {
            stamp,
            list,
            pages: JournalPages {
                file,
                at: pages_at,
                count,
            },
        }))
    }

    /// Writes a journal's pages, then `stamp` as the header, over the data file a page at a time,
    /// flushed: a checkpoint's second half, and recovery's finishing of it.
    fn apply(disk: &dyn Disk, stamp: &Stamp, pages: &JournalPages) -> Result<()> {
        let entry_len = to_u64(ENTRY_BYTES)?;
        let mut buf = vec![0u8; ENTRY_BYTES];
        for i in 0..pages.count {
            let at = i
                .checked_mul(entry_len)
                .and_then(|o| o.checked_add(pages.at))
                .ok_or(LimboError::IntegerOverflow)?;
            if disk.read_at(pages.file, at, &mut buf)? != ENTRY_BYTES {
                return Err(corrupt("a checkpoint journal's page past its file's end"));
            }
            let mut head = 0;
            let no = take_u64(&buf, &mut head)
                .filter(|no| (1..=stamp.allocated).contains(no))
                .ok_or_else(|| corrupt("a checkpoint journal's page past the pages handed out"))?;
            let page = buf
                .get(head..)
                .ok_or_else(|| corrupt("a checkpoint journal's page cut short"))?;
            disk.write_at(File::Data, page_offset(no)?, page)?;
        }
        Database::write_header(disk, stamp)?;
        disk.sync(File::Data)
    }
}

/// A versioned database on a disk.
pub struct Database {
    disk: Arc<dyn Disk>,
    pub v: Versioned,
    redo: Redo,
    /// Row changes hold it shared for their page changes; a checkpoint holds it exclusive to take
    /// its timestamp and copy.
    gate: RwLock<()>,
    /// The last checkpoint's sequence number; held by the checkpoint under way.
    last: Mutex<u64>,
    /// A column copy's feed, handed each commit's record under the publish lock (G11 decision 1,
    /// part 2): set once, by [`Database::keep_columns`].
    columns: std::sync::OnceLock<Arc<Feed>>,
    /// The tree that holds large values' chunks ([`overflow`], `AC-NS.2.4`): the one after the
    /// `trees` the database was made with, ordered by bytes.
    overflow: u64,
    /// Each tree's recorded order (G7 step 2, `AC-NS.2.1`), as its opener encoded it, by tree: one
    /// a tree ordered, gone when the tree is dropped. Each checkpoint's list names them all.
    orders: Mutex<HashMap<u64, Vec<u8>>>,
}

/// Orders the overflow tree's keys: by bytes, whatever the database's comparison.
fn bytes_order() -> Comparator {
    Box::new(|a: &[u8], b: &[u8]| a.cmp(b))
}

impl Database {
    /// The database `disk` holds, recovered (or made, with `trees` empty trees, if it holds
    /// none), in a pool of `frames` pages resident at most, the rest evicted to the file; its trees ordered by `cmp`, and after them the
    /// overflow tree (`AC-NS.2.4`). Reopened with the `trees` it was made with.
    pub fn open(
        disk: Arc<dyn Disk>,
        frames: usize,
        trees: usize,
        cmp: impl Fn() -> Comparator,
    ) -> Result<Self> {
        Self::open_ordered(disk, frames, trees, cmp, |_, _| {
            Err(corrupt(
                "a tree's recorded order, which this opener doesn't read",
            ))
        })
    }

    /// [`Database::open`], each tree whose order was recorded ([`Database::order_tree`]) ordered
    /// by `order` of its id and record, read ahead of the replay (G7 step 2, `AC-NS.2.1`); the others by
    /// `cmp`.
    pub fn open_ordered(
        disk: Arc<dyn Disk>,
        frames: usize,
        trees: usize,
        cmp: impl Fn() -> Comparator,
        order: impl Fn(u64, &[u8]) -> Result<Comparator>,
    ) -> Result<Self> {
        Self::open_in(disk, Room::Frames(frames), trees, cmp, order)
    }

    /// [`Database::open_ordered`], its pool's frames within `room`: a shared one's taken from its
    /// memory source as pages need them, its versions' bytes too (G7 step 2, `AC-NS.2.1`).
    pub fn open_in(
        disk: Arc<dyn Disk>,
        room: Room,
        trees: usize,
        cmp: impl Fn() -> Comparator,
        order: impl Fn(u64, &[u8]) -> Result<Comparator>,
    ) -> Result<Self> {
        let mut first = vec![0u8; PAGE_SIZE];
        let n = disk.read_at(File::Data, 0, &mut first)?;
        let header = first
            .get(..n)
            .and_then(|h| unseal(HEADER_MAGIC, h))
            .and_then(|body| Stamp::decode(body, &mut 0));
        // The header's checkpoint's journal, or the next one's if it was flushed before the power
        // went; with the header damaged, the newer of the two.
        let mut newest: Option<Journal> = None;
        for file in [File::Journal, File::JournalNext] {
            let Some(j) = Journal::read(&*disk, file)? else {
                continue;
            };
            let current = header.as_ref().is_none_or(|h| j.stamp.seq >= h.seq);
            if current && newest.as_ref().is_none_or(|o| j.stamp.seq > o.stamp.seq) {
                newest = Some(j);
            }
        }
        let Some(journal) = newest else {
            // Making a database flushes its journal before it writes the data file: with neither,
            // and no commit logged, it was never made.
            let logged = redo::read(&*disk)?;
            if header.is_none() && n == 0 && logged.iter().all(redo::Found::is_empty) {
                return Self::create(disk, room, trees, cmp);
            }
            return Err(corrupt(
                "no complete checkpoint journal at or after the data file's header",
            ));
        };
        Journal::apply(&*disk, &journal.stamp, &journal.pages)?;
        let Journal { stamp, list, .. } = journal;
        // A checkpoint that switched turns and never finished leaves commits after T in both.
        let replay = redo::merge(redo::read(&*disk)?, disk.lanes(), stamp.ts)?;
        redo::trim(&*disk, &replay.ends)?;
        // Pages are read in as they are asked for, evicted past the frames (G7 step 4).
        let pool = Pool::backed_in(room, disk.clone(), stamp.allocated)?;
        if stamp.roots.len() <= trees {
            return Err(corrupt("fewer trees than the database was opened with"));
        }
        // Each tree's order, read ahead of the replay: the checkpoint's list names the orders
        // recorded before it, the records after it the rest.
        let mut orders: HashMap<u64, Vec<u8>> = HashMap::new();
        for op in list
            .iter()
            .chain(replay.records.iter().flat_map(|r| r.ops.iter()))
        {
            match op {
                Op::Ordered { tree, order } => {
                    orders.insert(*tree, order.clone());
                }
                Op::Dropped { tree } => {
                    orders.remove(tree);
                }
                Op::Put { .. } | Op::Remove { .. } | Op::Made { .. } => {}
            }
        }
        let mut ordered: HashMap<u64, Comparator> = HashMap::with_capacity(orders.len());
        for (tree, bytes) in &orders {
            ordered.insert(*tree, order(*tree, bytes)?);
        }
        let ordered = Mutex::new(ordered);
        let comparator = |tree: u64| ordered.lock().remove(&tree).unwrap_or_else(&cmp);
        let overflow = u64::try_from(trees).map_err(|_| LimboError::IntegerOverflow)?;
        let v = Versioned::open(pool, &stamp.roots, |i| {
            if i == trees {
                bytes_order()
            } else {
                comparator(u64::try_from(i).unwrap_or(overflow))
            }
        })?;
        let changes = list
            .iter()
            .chain(replay.records.iter().flat_map(|r| r.ops.iter()));
        Self::apply(&v, changes, &comparator)?;
        v.clear_versions(&mut Descender::new())?;
        v.clock.recovered(replay.newest);
        // Each checkpoint switches the turn appends go to: after checkpoint `seq`, turn `seq % 2`.
        let turn = usize::from(!stamp.seq.is_multiple_of(2));
        Ok(Self {
            disk,
            v,
            redo: Redo::new(replay.ends, turn, replay.newest)?,
            gate: RwLock::new(()),
            last: Mutex::new(stamp.seq),
            columns: std::sync::OnceLock::new(),
            overflow,
            orders: Mutex::new(orders),
        })
    }

    /// Makes `changes` directly, with no versions (recovery): the row changes between two trees
    /// made or dropped side by side ([`Database::apply_rows`]), each tree made or dropped
    /// (`AC-NS.2.5`) once every change before it is made, a made tree ordered by `cmp` of its id.
    /// Orders were read ahead of the replay.
    fn apply<'a>(
        v: &Versioned,
        changes: impl Iterator<Item = &'a Op>,
        cmp: &dyn Fn(u64) -> Comparator,
    ) -> Result<()> {
        let mut rows = Vec::new();
        for op in changes {
            match op {
                Op::Made { tree } => {
                    Self::apply_rows(v, std::mem::take(&mut rows))?;
                    v.remake_tree(*tree, cmp(*tree))?;
                }
                Op::Dropped { tree } => {
                    Self::apply_rows(v, std::mem::take(&mut rows))?;
                    v.drop_tree(*tree)?;
                }
                Op::Put { .. } | Op::Remove { .. } => rows.push(op),
                Op::Ordered { .. } => {}
            }
        }
        Self::apply_rows(v, rows)
    }

    /// Makes row changes directly, with no versions (recovery), side by side: each row's changes in
    /// their order on one thread of as many as the machine runs (SiloR's parallel replay: Zheng,
    /// Tu, Liskov, Madden, OSDI 2014), so the rows end as one thread making them in order would
    /// leave them.
    fn apply_rows(v: &Versioned, changes: Vec<&Op>) -> Result<()> {
        if changes.is_empty() {
            return Ok(());
        }
        // No more replayers than the pool's frames can hold latched at once (G7 step 4): past
        // that, a pool whose every frame is latched refuses.
        let parts = std::thread::available_parallelism()
            .map_or(1, std::num::NonZero::get)
            .min(
                v.pool
                    .capacity()
                    .checked_div(super::blink::WRITER_LATCHES)
                    .unwrap_or(1),
            )
            .max(1);
        let mut by: Vec<Vec<&Op>> = vec![Vec::new(); parts];
        for op in changes {
            by.get_mut(partition(op, parts))
                .ok_or(LimboError::IntegerOverflow)?
                .push(op);
        }
        std::thread::scope(|scope| {
            let workers: Vec<_> = by
                .into_iter()
                .filter(|ops| !ops.is_empty())
                .map(|ops| {
                    scope.spawn(move || -> Result<()> {
                        let mut d = Descender::new();
                        for op in ops {
                            v.tree(Self::tree_of(op)?)?
                                .apply_plain(&v.pool, &mut d, op)?;
                        }
                        Ok(())
                    })
                })
                .collect();
            for w in workers {
                w.join().map_err(|_| {
                    LimboError::InternalError("versioned storage: a recovery replayer ended".into())
                })??;
            }
            Ok(())
        })
    }

    /// A new database of `trees` empty trees: checkpoint 0.
    fn create(
        disk: Arc<dyn Disk>,
        room: Room,
        trees: usize,
        cmp: impl Fn() -> Comparator,
    ) -> Result<Self> {
        let all = trees.checked_add(1).ok_or(LimboError::IntegerOverflow)?;
        let v = Versioned::new_in(Pool::backed_in(room, disk.clone(), 0)?, all, |i| {
            if i == trees {
                bytes_order()
            } else {
                cmp()
            }
        })?;
        let allocated = v.pool.allocated();
        let epoch = v.pool.begin_copy()?;
        let stamp = Stamp {
            seq: 0,
            ts: 0,
            allocated,
            roots: v.roots(),
        };
        // What the redo files hold is no record (the caller found none) of this database.
        let ends = vec![[0, 0]; disk.lanes()];
        redo::trim(&*disk, &ends)?;
        let pages = Journal::write(&*disk, &stamp, &[], |each| {
            v.pool.copy_owed(epoch, allocated, each)
        })?;
        Journal::apply(&*disk, &stamp, &pages)?;
        Ok(Self {
            disk,
            v,
            redo: Redo::new(ends, 0, 0)?,
            gate: RwLock::new(()),
            last: Mutex::new(0),
            columns: std::sync::OnceLock::new(),
            overflow: u64::try_from(trees).map_err(|_| LimboError::IntegerOverflow)?,
            orders: Mutex::new(HashMap::new()),
        })
    }

    /// The tree of a row change (a checkpoint's correction or a replayed one).
    fn tree_of(op: &Op) -> Result<u64> {
        match op {
            Op::Put { tree, .. } | Op::Remove { tree, .. } => Ok(*tree),
            Op::Made { .. } | Op::Dropped { .. } | Op::Ordered { .. } => Err(corrupt(
                "a row change naming a tree made, dropped or ordered",
            )),
        }
    }

    /// Makes an empty tree ordered by `cmp` (`AC-NS.2.5`), logged as a record of its own, durable
    /// before it returns: its id. Row changes and checkpoints wait for it. `cmp` must order as the
    /// comparison the database is opened with: recovery remakes the tree with that one, until the
    /// catalog names a comparison per tree (G7 step 2's `sqlite_schema`).
    pub fn make_tree(&self, d: &mut Descender, cmp: Comparator) -> Result<u64> {
        let _alone = self.gate.write();
        self.redo.usable()?;
        let id = self.v.make_tree(cmp)?;
        let logged = self.log_alone(d, Op::Made { tree: id });
        if logged.is_err() {
            self.v.drop_tree(id)?;
        }
        logged.map(|()| id)
    }

    /// Drops tree `tree` (`AC-NS.2.5`), logged as a record of its own, durable before it returns;
    /// refused (`Busy`) while a transaction still open has changed one of its rows. Its pages
    /// stay in the file, unreferenced: the pool keeps no free list yet. Row changes and
    /// checkpoints wait for it.
    pub fn drop_tree(&self, d: &mut Descender, tree: u64) -> Result<()> {
        let tree = self.user_tree(tree)?;
        let _alone = self.gate.write();
        self.redo.usable()?;
        if self.v.open_changes(tree)? {
            return Err(LimboError::Busy);
        }
        self.log_alone(d, Op::Dropped { tree })?;
        self.orders.lock().remove(&tree);
        self.v.drop_tree(tree)
    }

    /// Records tree `tree`'s order, `order` as the opener encodes it (G7 step 2, `AC-NS.2.1`),
    /// logged as a record of its own, durable before it returns: given before any row of the tree
    /// is written, so recovery orders the tree by it (`open_ordered`'s `order`) before it replays
    /// one. Row changes and checkpoints wait for it.
    pub fn order_tree(&self, d: &mut Descender, tree: u64, order: Vec<u8>) -> Result<()> {
        let tree = self.user_tree(tree)?;
        let _alone = self.gate.write();
        self.redo.usable()?;
        let op = Op::Ordered { tree, order };
        self.log_alone(d, op.clone())?;
        if let Op::Ordered { tree, order } = op {
            self.orders.lock().insert(tree, order);
        }
        Ok(())
    }

    /// Logs `op` alone at the next timestamp, durable, under the gate held exclusive.
    fn log_alone(&self, d: &mut Descender, op: Op) -> Result<()> {
        let txn = self.v.clock.begin()?;
        let lane = self.redo.lane();
        let ts = self.v.commit_logged(txn, d, |ts, _| {
            self.redo.append(lane, ts, std::slice::from_ref(&op))
        })?;
        self.redo.sync(&*self.disk, lane, ts)
    }

    fn write_header(disk: &dyn Disk, stamp: &Stamp) -> Result<()> {
        let mut body = Vec::new();
        stamp.encode(&mut body)?;
        let mut page = seal(HEADER_MAGIC, &body)?;
        if page.len() > PAGE_SIZE {
            return Err(LimboError::TooBig);
        }
        page.resize(PAGE_SIZE, 0);
        disk.write_at(File::Data, 0, &page)
    }

    pub fn begin(&self) -> Result<Txn> {
        self.v.clock.begin()
    }

    pub fn snapshot(&self) -> Result<Snapshot> {
        self.v.clock.snapshot()
    }

    pub fn close_snapshot(&self, snap: Snapshot) {
        self.v.clock.close(snap.start);
    }

    /// A tree the caller may name: any but the overflow tree, whose rows are the database's own.
    fn user_tree(&self, tree: u64) -> Result<u64> {
        if tree == self.overflow {
            return Err(LimboError::InternalError(format!(
                "versioned storage: tree {tree} is the overflow tree"
            )));
        }
        Ok(tree)
    }

    /// The row under `key` in `tree` as `snap` sees it, its chunks put back after its cell's
    /// value (`AC-NS.2.4`).
    pub fn get(
        &self,
        d: &mut Descender,
        snap: &Snapshot,
        tree: u64,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        let tree = self.user_tree(tree)?;
        let Some(mut value) = self
            .v
            .tree(tree)?
            .get(&self.v.pool, d, &self.v.clock, snap, key)?
        else {
            return Ok(None);
        };
        self.join(d, snap, tree, key, &mut value)?;
        Ok(Some(value))
    }

    /// Appends to `value`, a cell's value of the row under `key` in `tree`, the chunks `snap`
    /// sees, if it continues in any (`AC-NS.2.4`): chunk 0 on, to the first absent.
    pub fn join(
        &self,
        d: &mut Descender,
        snap: &Snapshot,
        tree: u64,
        key: &[u8],
        value: &mut Vec<u8>,
    ) -> Result<()> {
        if !overflow::continues(key, value) {
            return Ok(());
        }
        let chunks = self.v.tree(self.overflow)?;
        for i in 0..=overflow::max_chunks(key)? {
            let ck = overflow::chunk_key(tree, key, i);
            match chunks.get(&self.v.pool, d, &self.v.clock, snap, &ck)? {
                Some(chunk) => value.extend_from_slice(&chunk),
                None => return Ok(()),
            }
        }
        Err(corrupt(
            "a row with more chunks than its longest value takes",
        ))
    }

    /// Up to `most` rows of `tree` from `from` on as `snap` sees them, in key order, each with its
    /// chunks put back (`AC-NS.2.4`).
    pub fn scan(
        &self,
        d: &mut Descender,
        snap: &Snapshot,
        tree: u64,
        from: Option<&[u8]>,
        most: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let tree = self.user_tree(tree)?;
        let mut rows = self
            .v
            .tree(tree)?
            .scan(&self.v.pool, d, &self.v.clock, snap, from, most)?;
        for (key, value) in &mut rows {
            self.join(d, snap, tree, key, value)?;
        }
        Ok(rows)
    }

    /// `txn` puts `value` under `key` in `tree`, its tail past a cell's room as chunks in the
    /// overflow tree (`AC-NS.2.4`), the previous row's chunks past the new one's removed: the row it
    /// saw there before, if any, whole.
    pub fn put(
        &self,
        d: &mut Descender,
        txn: &mut Txn,
        tree: u64,
        key: &[u8],
        value: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        let tree = self.user_tree(tree)?;
        let (head, chunks) = overflow::split(key, value)?;
        let _changing = self.gate.read();
        let before = self
            .v
            .tree(tree)?
            .put(&self.v.pool, d, &self.v.clock, txn, key, head)?;
        let had = before.as_ref().is_some_and(|b| overflow::continues(key, b));
        let mut before = before;
        let store = self.v.tree(self.overflow)?;
        let mut i = 0u32;
        for chunk in chunks {
            let ck = overflow::chunk_key(tree, key, i);
            let old = store.put(&self.v.pool, d, &self.v.clock, txn, &ck, chunk)?;
            if let (true, Some(b), Some(old)) = (had, before.as_mut(), old) {
                b.extend_from_slice(&old);
            }
            i = i.checked_add(1).ok_or(LimboError::IntegerOverflow)?;
        }
        if had {
            self.remove_chunks(d, txn, tree, key, i, before.as_mut())?;
        }
        Ok(before)
    }

    /// `txn` removes the chunks of the row under `key` in `tree` from chunk `from` on, to the first
    /// absent; each removed appended to `into`.
    fn remove_chunks(
        &self,
        d: &mut Descender,
        txn: &mut Txn,
        tree: u64,
        key: &[u8],
        from: u32,
        mut into: Option<&mut Vec<u8>>,
    ) -> Result<()> {
        let store = self.v.tree(self.overflow)?;
        for i in from..=overflow::max_chunks(key)? {
            let ck = overflow::chunk_key(tree, key, i);
            match store.remove(&self.v.pool, d, &self.v.clock, txn, &ck)? {
                Some(old) => {
                    if let Some(b) = into.as_deref_mut() {
                        b.extend_from_slice(&old);
                    }
                }
                None => return Ok(()),
            }
        }
        Err(corrupt(
            "a row with more chunks than its longest value takes",
        ))
    }

    /// `txn` removes the row under `key` in `tree` and its chunks (`AC-NS.2.4`): the row it saw
    /// there, if any, whole.
    pub fn remove(
        &self,
        d: &mut Descender,
        txn: &mut Txn,
        tree: u64,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        let tree = self.user_tree(tree)?;
        let _changing = self.gate.read();
        let mut before = self
            .v
            .tree(tree)?
            .remove(&self.v.pool, d, &self.v.clock, txn, key)?;
        if before.as_ref().is_some_and(|b| overflow::continues(key, b)) {
            self.remove_chunks(d, txn, tree, key, 0, before.as_mut())?;
        }
        Ok(before)
    }

    /// A redo lane for a writer to commit on: its own while there are as many lanes as writers.
    pub fn lane(&self) -> LaneId {
        self.redo.lane()
    }

    /// Commits `txn`, logged to `lane`; durable before it returns if `durable` (every commit up to
    /// it durable), else once a later [`Database::sync`] returns. Its timestamp.
    pub fn commit(&self, txn: Txn, d: &mut Descender, lane: LaneId, durable: bool) -> Result<Ts> {
        // Shared for the publish, which a checkpoint's timestamp must not split, and for a
        // rollback a failed append makes, which changes pages.
        let ts = {
            let _changing = self.gate.read();
            self.v.commit_logged(txn, d, |ts, ops| {
                self.redo.append(lane, ts, ops)?;
                if let Some(feed) = self.columns.get() {
                    feed.offer(ts, &self.whole(ops)?);
                }
                Ok(())
            })?
        };
        if durable {
            self.redo.sync(&*self.disk, lane, ts)?;
        }
        Ok(ts)
    }

    /// `ops` as a column copy reads them (`AC-NS.2.4`): each put of a value that continues in
    /// chunks with the chunks put after it in the same commit (every put of a long value writes
    /// all of its chunks, in order, right after its cell), the overflow tree's own changes left
    /// out.
    fn whole(&self, ops: &[Op]) -> Result<Vec<Op>> {
        // Each continuing put's chunk key prefix (its tree and key), to where it is in `out`.
        let mut open: rustc_hash::FxHashMap<Vec<u8>, usize> = rustc_hash::FxHashMap::default();
        let mut out: Vec<Op> = Vec::with_capacity(ops.len());
        for op in ops {
            match op {
                Op::Put { tree, key, value } if *tree == self.overflow => {
                    let prefix = key.get(..key.len().saturating_sub(4)).unwrap_or_default();
                    if let Some(Op::Put { value: whole, .. }) =
                        open.get(prefix).and_then(|&at| out.get_mut(at))
                    {
                        whole.extend_from_slice(value);
                    }
                }
                Op::Remove { tree, .. } if *tree == self.overflow => {}
                Op::Put { tree, key, value } => {
                    let mut prefix = overflow::chunk_key(*tree, key, 0);
                    prefix.truncate(prefix.len().saturating_sub(4));
                    if overflow::continues(key, value) {
                        open.insert(prefix, out.len());
                    } else {
                        open.remove(&prefix);
                    }
                    out.push(op.clone());
                }
                Op::Remove { tree, key } => {
                    let mut prefix = overflow::chunk_key(*tree, key, 0);
                    prefix.truncate(prefix.len().saturating_sub(4));
                    open.remove(&prefix);
                    out.push(op.clone());
                }
                Op::Made { .. } | Op::Dropped { .. } | Op::Ordered { .. } => out.push(op.clone()),
            }
        }
        Ok(out)
    }

    /// Rolls `txn` back to `sp` ([`Versioned::rollback_to`]).
    pub fn rollback_to(&self, d: &mut Descender, txn: &mut Txn, sp: Savepoint) -> Result<()> {
        let _changing = self.gate.read();
        self.v.rollback_to(txn, sp, d)
    }

    /// From the next commit on, hands each commit's record to `feed` as it is appended to the redo
    /// log (G11 decision 1, part 2). One feed a database: a second is refused.
    pub fn keep_columns(&self, feed: Arc<Feed>) -> Result<()> {
        self.columns.set(feed).map_err(|_| {
            LimboError::InternalError("versioned storage: a column copy is already kept".into())
        })
    }

    /// The newest commit published: what a column copy kept as a file must be current as of.
    pub fn newest(&self) -> Ts {
        self.v.clock.at_publish(|ts| ts)
    }

    /// Drops the versions no open snapshot can see ([`Versioned::collect`]): how many rows lost
    /// their heads. It changes pages (a removed row's cell goes), so it holds the gate shared, as
    /// every page change does: a checkpoint's pause sees no page change under way.
    pub fn collect(&self, d: &mut Descender) -> Result<usize> {
        let _changing = self.gate.read();
        self.v.collect(d)
    }

    pub fn abort(&self, txn: Txn, d: &mut Descender) -> Result<()> {
        let _changing = self.gate.read();
        self.v.abort(txn, d)
    }

    /// The redo log's flushes so far and the records they carried.
    pub fn log_counters(&self) -> redo::Counters {
        self.redo.counters()
    }

    /// Makes every commit so far durable: the newest durable commit.
    pub fn sync(&self) -> Result<Ts> {
        let ts = self.v.clock.at_publish(|ts| ts);
        self.redo.sync_all(&*self.disk, ts)?;
        Ok(ts)
    }

    /// A checkpoint (decision 7): the pages written since the last, the rows to correct, the
    /// redo log before its timestamp cut. Its timestamp. One that fails leaves the database
    /// making nothing more durable until reopened.
    pub fn checkpoint(&self) -> Result<Ts> {
        let mut last = self.last.lock();
        self.redo.usable()?;
        let seq = last.checked_add(1).ok_or(LimboError::IntegerOverflow)?;
        match self.checkpoint_as(seq) {
            Ok(ts) => {
                *last = seq;
                Ok(ts)
            }
            Err(e) => {
                // The pages it took as copied may not have reached the file.
                self.redo.poison();
                Err(e)
            }
        }
    }

    fn checkpoint_as(&self, seq: u64) -> Result<Ts> {
        // The pause: no page changes and no commit publishes while it lasts. What it takes is
        // bounded by the rows with versions, not by the pages written.
        let (stamp, list, epoch, from) = {
            let _alone = self.gate.write();
            let (ts, from) = self.v.clock.at_publish(|ts| (ts, self.redo.switch()));
            let stamp = Stamp {
                seq,
                ts,
                allocated: self.v.pool.allocated(),
                roots: self.v.roots(),
            };
            // The trees' orders go in the list beside the corrections: the redo records naming
            // them are cut once the checkpoint is written.
            let mut list = self.v.corrections(ts)?;
            list.extend(self.orders.lock().iter().map(|(tree, order)| Op::Ordered {
                tree: *tree,
                order: order.clone(),
            }));
            (stamp, list, self.v.pool.begin_copy()?, from)
        };
        // Off the commit path: writers go on, each keeping a page's bytes at the pause before its
        // first change of it.
        // Copied a page at a time into the journal: what it holds is a page, not the pages written.
        let pages = Journal::write(&*self.disk, &stamp, &list, |each| {
            self.v.pool.copy_owed(epoch, stamp.allocated, each)
        })?;
        Journal::apply(&*self.disk, &stamp, &pages)?;
        // Commits up to T are in the file now; the redo file switched from holds no others.
        self.redo.cut(&*self.disk, from, stamp.ts)?;
        Ok(stamp.ts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::versioned::disk::Simulated;
    use std::collections::BTreeMap;

    fn cmp() -> Comparator {
        Box::new(|a: &[u8], b: &[u8]| a.cmp(b))
    }

    fn rows(db: &Database) -> BTreeMap<Vec<u8>, Vec<u8>> {
        let mut d = Descender::new();
        let snap = db.snapshot().unwrap();
        let all = db.scan(&mut d, &snap, 0, None, usize::MAX).unwrap();
        db.close_snapshot(snap);
        all.into_iter().collect()
    }

    /// Do: commit rows durably, checkpoint, commit more, reopen on the same disk; then cut the
    /// power after unsynced commits and reopen. Expect: every durable commit's rows after each
    /// reopen; an unsynced commit's rows, if the power cut kept its record whole, else none of
    /// them (T-T1.3.1).
    #[test]
    fn durable_commits_survive_a_reopen_and_a_power_cut() {
        let disk = Arc::new(Simulated::new(5, 1).unwrap());
        let mut want = BTreeMap::new();
        {
            let db = Database::open(disk.clone(), 2_000, 1, cmp).unwrap();
            let mut d = Descender::new();
            for round in 0..3u8 {
                let mut t = db.begin().unwrap();
                for k in 0..50u8 {
                    db.put(&mut d, &mut t, 0, &[k], &[round]).unwrap();
                    want.insert(vec![k], vec![round]);
                }
                db.commit(t, &mut d, db.lane(), true).unwrap();
                if round == 1 {
                    db.checkpoint().unwrap();
                }
            }
        }
        let db = Database::open(disk.clone(), 2_000, 1, cmp).unwrap();
        assert_eq!(rows(&db), want);
        let mut d = Descender::new();
        let mut t = db.begin().unwrap();
        db.put(&mut d, &mut t, 0, b"late", b"x").unwrap();
        db.commit(t, &mut d, db.lane(), false).unwrap();
        drop(db);
        disk.power_cut().unwrap();
        let db = Database::open(disk, 2_000, 1, cmp).unwrap();
        let got = rows(&db);
        let mut with_late = want.clone();
        with_late.insert(b"late".to_vec(), b"x".to_vec());
        assert!(got == want || got == with_late, "{} rows", got.len());
    }

    /// Do: a transaction left open across a checkpoint, then committed after it; another open
    /// across one and rolled back; reopen. Expect: the committed one's rows, not the rolled-back
    /// one's: the checkpoint's list put them back to their versions as of its timestamp and the
    /// redo log replayed the commit after it (T-T1.3.1).
    #[test]
    fn a_checkpoint_mid_transaction_keeps_only_what_commits() {
        let disk = Arc::new(Simulated::new(9, 1).unwrap());
        let db = Database::open(disk.clone(), 2_000, 1, cmp).unwrap();
        let mut d = Descender::new();
        let mut base = db.begin().unwrap();
        db.put(&mut d, &mut base, 0, b"a", b"0").unwrap();
        db.put(&mut d, &mut base, 0, b"b", b"0").unwrap();
        db.commit(base, &mut d, db.lane(), true).unwrap();
        let mut kept = db.begin().unwrap();
        db.put(&mut d, &mut kept, 0, b"a", b"1").unwrap();
        let mut lost = db.begin().unwrap();
        db.put(&mut d, &mut lost, 0, b"b", b"9").unwrap();
        db.put(&mut d, &mut lost, 0, b"c", b"9").unwrap();
        db.checkpoint().unwrap();
        db.commit(kept, &mut d, db.lane(), true).unwrap();
        db.abort(lost, &mut d).unwrap();
        drop(db);
        disk.power_cut().unwrap();
        let db = Database::open(disk, 2_000, 1, cmp).unwrap();
        let want: BTreeMap<_, _> = [
            (b"a".to_vec(), b"1".to_vec()),
            (b"b".to_vec(), b"0".to_vec()),
        ]
        .into_iter()
        .collect();
        assert_eq!(rows(&db), want);
    }

    /// Do: commit rows and checkpoint, twice; write zeros over the page the second checkpoint
    /// wrote, as a power cut that kept its header and not its page leaves the file; reopen.
    /// Expect: the rows as last committed: recovery writes the newest journal's pages again
    /// (T-T1.3.1).
    #[test]
    fn a_checkpoint_whose_header_outlived_its_pages_is_finished_from_its_journal() {
        let disk = Arc::new(Simulated::new(11, 1).unwrap());
        let mut want = BTreeMap::new();
        {
            let db = Database::open(disk.clone(), 2_000, 1, cmp).unwrap();
            let mut d = Descender::new();
            for round in 0..2u8 {
                let mut t = db.begin().unwrap();
                for k in 0..40u8 {
                    db.put(&mut d, &mut t, 0, &[k], &[round]).unwrap();
                    want.insert(vec![k], vec![round]);
                }
                db.commit(t, &mut d, db.lane(), true).unwrap();
                db.checkpoint().unwrap();
            }
            assert_eq!(
                db.v.pool.allocated(),
                2,
                "the rows fit the root leaf; page 2 is the overflow tree's root"
            );
        }
        disk.write_at(File::Data, page_offset(1).unwrap(), &[0u8; PAGE_SIZE])
            .unwrap();
        disk.sync(File::Data).unwrap();
        let db = Database::open(disk, 2_000, 1, cmp).unwrap();
        assert_eq!(rows(&db), want);
    }

    /// Do: commit rows and checkpoint; damage the data file's header and reopen; then empty both
    /// journals, damage the header again and reopen. Expect: the rows, from the journal, the first
    /// time; a refusal (`Corrupt`) the second, never a new, empty database over the old one's file
    /// (T-T1.3.1).
    #[test]
    fn a_damaged_header_is_recovered_from_its_journal_and_never_made_anew() {
        let disk = Arc::new(Simulated::new(13, 1).unwrap());
        let mut want = BTreeMap::new();
        {
            let db = Database::open(disk.clone(), 2_000, 1, cmp).unwrap();
            let mut d = Descender::new();
            let mut t = db.begin().unwrap();
            for k in 0..20u8 {
                db.put(&mut d, &mut t, 0, &[k], &[k]).unwrap();
                want.insert(vec![k], vec![k]);
            }
            db.commit(t, &mut d, db.lane(), true).unwrap();
            db.checkpoint().unwrap();
        }
        disk.write_at(File::Data, 20, &[0xff; 8]).unwrap();
        disk.sync(File::Data).unwrap();
        let db = Database::open(disk.clone(), 2_000, 1, cmp).unwrap();
        assert_eq!(rows(&db), want);
        drop(db);
        for file in [File::Journal, File::JournalNext] {
            disk.truncate(file, 0).unwrap();
            disk.sync(file).unwrap();
        }
        disk.write_at(File::Data, 20, &[0xff; 8]).unwrap();
        disk.sync(File::Data).unwrap();
        assert!(matches!(
            Database::open(disk, 2_000, 1, cmp),
            Err(LimboError::Corrupt(_))
        ));
    }

    /// Do: a checkpoint on a disk that fails partway, then a commit, not asked to be durable.
    /// Expect: both refused and the commit rolled back, since the pages the checkpoint took as
    /// copied may not be on the disk; after a power cut, the rows committed before (T-T1.3.1).
    #[test]
    fn a_failed_checkpoint_refuses_commits_until_reopened() {
        let disk = Arc::new(Simulated::new(17, 1).unwrap());
        let db = Database::open(disk.clone(), 2_000, 1, cmp).unwrap();
        let mut d = Descender::new();
        let mut t = db.begin().unwrap();
        db.put(&mut d, &mut t, 0, b"a", b"1").unwrap();
        db.commit(t, &mut d, db.lane(), true).unwrap();
        let mut t = db.begin().unwrap();
        db.put(&mut d, &mut t, 0, b"b", b"1").unwrap();
        // The journal's cut, its head and its first pages: the next write fails.
        disk.crash_after(4);
        assert!(db.checkpoint().is_err());
        assert!(db.commit(t, &mut d, db.lane(), false).is_err());
        let snap = db.snapshot().unwrap();
        assert_eq!(db.get(&mut d, &snap, 0, b"b").unwrap(), None);
        db.close_snapshot(snap);
        drop(db);
        disk.power_cut().unwrap();
        let db = Database::open(disk, 2_000, 1, cmp).unwrap();
        let want: BTreeMap<_, _> = [(b"a".to_vec(), b"1".to_vec())].into_iter().collect();
        assert_eq!(rows(&db), want);
    }

    /// Do: commit a row durably; after its record, put what a torn flush may leave: bytes as many
    /// as the next commit's record, then a whole record from an earlier life of the file; reopen,
    /// commit that next record durably, reopen. Expect: the rows committed and not the old
    /// record's: recovery cut the file after its last whole record, so the next append didn't
    /// line the old record up behind it (T-T1.3.1).
    #[test]
    fn a_torn_redo_tail_is_cut_before_appends_reach_it() {
        let disk = Arc::new(Simulated::new(19, 1).unwrap());
        {
            let db = Database::open(disk.clone(), 2_000, 1, cmp).unwrap();
            let mut d = Descender::new();
            let mut t = db.begin().unwrap();
            db.put(&mut d, &mut t, 0, b"a", b"1").unwrap();
            db.commit(t, &mut d, db.lane(), true).unwrap();
        }
        let put = |key: &[u8], value: &[u8]| Op::Put {
            tree: 0,
            key: key.to_vec(),
            value: value.to_vec(),
        };
        let next = redo::encode(2, &[put(b"b", b"2")]).unwrap();
        let old = redo::encode(3, &[put(b"a", b"old")]).unwrap();
        let mut tail = vec![0xee; next.len()];
        tail.extend_from_slice(&old);
        let redo = File::Redo { lane: 0, turn: 0 };
        let end = disk.len(redo).unwrap();
        disk.write_at(redo, end, &tail).unwrap();
        disk.sync(redo).unwrap();
        {
            let db = Database::open(disk.clone(), 2_000, 1, cmp).unwrap();
            let mut d = Descender::new();
            let mut t = db.begin().unwrap();
            db.put(&mut d, &mut t, 0, b"b", b"2").unwrap();
            assert_eq!(
                db.commit(t, &mut d, db.lane(), true).unwrap(),
                2,
                "the record is `next`"
            );
        }
        let db = Database::open(disk, 2_000, 1, cmp).unwrap();
        let want: BTreeMap<_, _> = [
            (b"a".to_vec(), b"1".to_vec()),
            (b"b".to_vec(), b"2".to_vec()),
        ]
        .into_iter()
        .collect();
        assert_eq!(rows(&db), want);
    }

    /// Do: 8 threads commit to their own keys, every other commit durable, one of them
    /// checkpointing every 20 commits while the rest commit on (copying pages as they change them);
    /// one log, then a lane each; cut the power and reopen. Expect: the rows of every commit up to
    /// some timestamp, applied in timestamp order, at least up to the last commit acknowledged
    /// durable (T-T1.3.1).
    #[test]
    fn commits_racing_checkpoints_recover_a_prefix() {
        for lanes in [1, 8] {
            commits_racing_checkpoints(lanes);
        }
    }

    fn commits_racing_checkpoints(lanes: usize) {
        let disk = Arc::new(Simulated::new(23, lanes).unwrap());
        let db = Arc::new(Database::open(disk.clone(), 20_000, 1, cmp).unwrap());
        let writers: Vec<_> = (0..8u8)
            .map(|w| {
                let db = db.clone();
                std::thread::spawn(move || {
                    let mut d = Descender::new();
                    let lane = db.lane();
                    let mut done = Vec::new();
                    for i in 0..200u32 {
                        let mut t = db.begin().unwrap();
                        let key = vec![w, u8::try_from(i % 16).unwrap()];
                        let value = i.to_be_bytes().to_vec();
                        db.put(&mut d, &mut t, 0, &key, &value).unwrap();
                        let durable = i.is_multiple_of(2);
                        let ts = db.commit(t, &mut d, lane, durable).unwrap();
                        done.push((ts, key, value, durable));
                        if w == 0 && i % 20 == 19 {
                            db.checkpoint().unwrap();
                        }
                    }
                    done
                })
            })
            .collect();
        let mut commits: Vec<_> = writers
            .into_iter()
            .flat_map(|w| w.join().unwrap())
            .collect();
        drop(db);
        disk.power_cut().unwrap();
        let db = Database::open(disk, 20_000, 1, cmp).unwrap();
        let got = rows(&db);
        commits.sort_by_key(|c| c.0);
        let acknowledged = commits.iter().filter(|c| c.3).map(|c| c.0).max().unwrap();
        let mut state = BTreeMap::new();
        let mut matched = if state == got { Some(0) } else { None };
        for (ts, key, value, _) in &commits {
            state.insert(key.clone(), value.clone());
            if state == got {
                matched = Some(*ts);
            }
        }
        let n = matched.expect("the rows match no prefix of the commits");
        assert!(
            n >= acknowledged,
            "{lanes} lanes: {n} recovered, {acknowledged} acknowledged"
        );
    }

    /// Do: for many crash points, run a workload of commits (durable, some not) and checkpoints on
    /// a disk that crashes after that many operations, then cut the power and reopen; on one log
    /// and on three lanes, commits taking the lanes in turn. Expect: the rows after reopening are
    /// exactly those of a prefix of the commits, at least as long as the last commit acknowledged
    /// durable (T-T1.3.1).
    #[test]
    fn a_crash_at_any_operation_recovers_a_prefix_of_commits() {
        crash_recovers_prefixes((5..400).step_by(7), |_| 30);
    }

    /// Do: the crash workload above with values from a few bytes to several pages, so rows'
    /// chunks in the overflow tree are written, replaced and removed by the commits, crashing at
    /// each of many operations. Expect: each reopen's rows, chunks put back, exactly a prefix of
    /// the commits at least as long as the last acknowledged durable (AC-NS.2.4, T-T1.3.1).
    #[test]
    fn ns2_4_large_values_recover_a_prefix_at_any_crash() {
        let room = overflow::inline_len(&[0]).unwrap();
        crash_recovers_prefixes((5..800).step_by(13), move |seed| match seed % 4 {
            0 => 30,
            1 => room,
            2 => room + 1 + usize::try_from(seed % 64).unwrap(),
            _ => 3 * PAGE_SIZE + usize::try_from(seed % 997).unwrap(),
        });
    }

    /// The crash workload: for each crash point, on one log and on three lanes, commits of puts
    /// (values of `len(seed)` bytes) and removals, some durable, and checkpoints, on a disk that
    /// crashes after that many operations; then a power cut and a reopen, whose rows must be a
    /// prefix of the commits at least as long as the last acknowledged.
    fn crash_recovers_prefixes(crashes: impl Iterator<Item = u64>, len: impl Fn(u64) -> usize) {
        let mut checked = 0;
        for (crash, lanes) in crashes.flat_map(|c| [(c, 1), (c, 3)]) {
            let disk = Arc::new(Simulated::new(crash, lanes).unwrap());
            let mut commits: Vec<BTreeMap<Vec<u8>, Option<Vec<u8>>>> = Vec::new();
            let mut acknowledged = 0usize;
            {
                let db = Database::open(disk.clone(), 4_000, 1, cmp).unwrap();
                let taken: Vec<_> = (0..lanes).map(|_| db.lane()).collect();
                disk.crash_after(crash);
                let mut d = Descender::new();
                let mut seed = crash;
                'run: for i in 0..60u64 {
                    let mut t = db.begin().unwrap();
                    let mut changes = BTreeMap::new();
                    for _ in 0..5 {
                        seed = seed
                            .wrapping_mul(6364136223846793005)
                            .wrapping_add(1442695040888963407);
                        let k = vec![u8::try_from((seed >> 33) % 40).unwrap()];
                        if (seed >> 60).is_multiple_of(4) {
                            if db.remove(&mut d, &mut t, 0, &k).is_err() {
                                break 'run;
                            }
                            changes.insert(k, None);
                        } else {
                            let v = vec![u8::try_from(i).unwrap(); len(seed >> 40)];
                            if db.put(&mut d, &mut t, 0, &k, &v).is_err() {
                                break 'run;
                            }
                            changes.insert(k, Some(v));
                        }
                    }
                    commits.push(changes);
                    let lane = taken[usize::try_from(i).unwrap() % lanes];
                    match db.commit(t, &mut d, lane, !i.is_multiple_of(3)) {
                        Ok(_) if !i.is_multiple_of(3) => acknowledged = commits.len(),
                        Ok(_) => {}
                        Err(_) => break 'run,
                    }
                    if i % 10 == 9 && db.checkpoint().is_err() {
                        break 'run;
                    }
                }
            }
            disk.power_cut().unwrap();
            let db = Database::open(disk.clone(), 4_000, 1, cmp)
                .unwrap_or_else(|e| panic!("crash {crash}, {lanes} lanes: reopen: {e}"));
            let got = rows(&db);
            let mut state: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
            let mut prefixes = vec![state.clone()];
            for c in &commits {
                for (k, v) in c {
                    match v {
                        Some(v) => state.insert(k.clone(), v.clone()),
                        None => state.remove(k),
                    };
                }
                prefixes.push(state.clone());
            }
            let n = prefixes.iter().position(|p| p == &got).unwrap_or_else(|| {
                panic!("crash {crash}, {lanes} lanes: rows match no prefix of the commits")
            });
            assert!(
                n >= acknowledged,
                "crash {crash}, {lanes} lanes: {n} commits recovered, {acknowledged} acknowledged"
            );
            checked += 1;
        }
        assert!(checked > 100);
    }

    /// Every tree's rows from id 2 on (after tree 0 and the overflow tree) up to `ids`: None for a
    /// tree that isn't there.
    fn made_trees(db: &Database, ids: u64) -> Vec<Option<BTreeMap<Vec<u8>, Vec<u8>>>> {
        let mut d = Descender::new();
        let snap = db.snapshot().unwrap();
        let out = (2..ids)
            .map(|t| {
                db.scan(&mut d, &snap, t, None, usize::MAX)
                    .ok()
                    .map(|rows| rows.into_iter().collect())
            })
            .collect();
        db.close_snapshot(snap);
        out
    }

    /// Do: make trees and write rows to them, drop some, checkpoint between, then reopen, and
    /// reopen again after a power cut. Expect: the trees made and not dropped with their rows, the
    /// dropped ones refused, a write to a dropped tree refused, the next tree made under the next
    /// id; a drop refused (`Busy`) while an open transaction has changed its rows (AC-NS.2.5).
    #[test]
    fn ns2_5_trees_made_and_dropped_survive_reopens() {
        let disk = Arc::new(Simulated::new(31, 1).unwrap());
        let want;
        {
            let db = Database::open(disk.clone(), 2_000, 1, cmp).unwrap();
            let mut d = Descender::new();
            let a = db.make_tree(&mut d, cmp()).unwrap();
            let b = db.make_tree(&mut d, cmp()).unwrap();
            assert_eq!((a, b), (2, 3));
            for (t, n) in [(a, 1u8), (b, 2)] {
                let mut txn = db.begin().unwrap();
                db.put(&mut d, &mut txn, t, b"k", &[n; 3]).unwrap();
                db.put(&mut d, &mut txn, t, b"long", &vec![n; 3 * PAGE_SIZE])
                    .unwrap();
                db.commit(txn, &mut d, db.lane(), true).unwrap();
            }
            let mut open = db.begin().unwrap();
            db.put(&mut d, &mut open, a, b"k", b"open").unwrap();
            assert!(matches!(db.drop_tree(&mut d, a), Err(LimboError::Busy)));
            db.abort(open, &mut d).unwrap();
            db.drop_tree(&mut d, a).unwrap();
            let mut txn = db.begin().unwrap();
            assert!(db.put(&mut d, &mut txn, a, b"k", b"x").is_err());
            db.abort(txn, &mut d).unwrap();
            db.checkpoint().unwrap();
            let c = db.make_tree(&mut d, cmp()).unwrap();
            assert_eq!(c, 4);
            let mut txn = db.begin().unwrap();
            db.put(&mut d, &mut txn, c, b"c", b"3").unwrap();
            db.commit(txn, &mut d, db.lane(), true).unwrap();
            db.drop_tree(&mut d, b).unwrap();
            want = made_trees(&db, 6);
            assert_eq!(want[0], None);
            assert_eq!(want[1], None);
            assert_eq!(want[2].as_ref().map(BTreeMap::len), Some(1));
            assert_eq!(want[3], None);
        }
        let db = Database::open(disk.clone(), 2_000, 1, cmp).unwrap();
        assert_eq!(made_trees(&db, 6), want);
        drop(db);
        disk.power_cut().unwrap();
        let db = Database::open(disk, 2_000, 1, cmp).unwrap();
        assert_eq!(made_trees(&db, 6), want);
        let mut d = Descender::new();
        assert_eq!(db.make_tree(&mut d, cmp()).unwrap(), 5);
    }

    fn reversed() -> Comparator {
        Box::new(|a: &[u8], b: &[u8]| b.cmp(a))
    }

    /// The comparison of a recorded order: `rev` orders keys from the last to the first.
    fn recorded(_tree: u64, order: &[u8]) -> Result<Comparator> {
        if order == b"rev" {
            Ok(reversed())
        } else {
            Err(LimboError::Corrupt(
                "an order this test doesn't know".into(),
            ))
        }
    }

    fn keys(db: &Database, tree: u64) -> Vec<Vec<u8>> {
        let mut d = Descender::new();
        let snap = db.snapshot().unwrap();
        let all = db.scan(&mut d, &snap, tree, None, usize::MAX).unwrap();
        db.close_snapshot(snap);
        all.into_iter().map(|(k, _)| k).collect()
    }

    /// Do (`AC-NS.2.1`, G7 step 2): make two trees ordered from the last key to the first, record
    /// each one's order, one before a checkpoint (which cuts the record from the redo log) and one
    /// after; write rows to both; reopen, write a key between, and reopen after a power cut. Expect:
    /// each tree's keys read back in its recorded order after each reopen, the key written after a
    /// reopen in its place; an order the opener doesn't read refused (`Corrupt`), not taken as the
    /// default.
    #[test]
    fn ns2_1_a_trees_recorded_order_survives_checkpoints_and_reopens() {
        let disk = Arc::new(Simulated::new(37, 1).unwrap());
        let (a, b);
        {
            let db = Database::open_ordered(disk.clone(), 2_000, 1, cmp, recorded).unwrap();
            let mut d = Descender::new();
            a = db.make_tree(&mut d, reversed()).unwrap();
            db.order_tree(&mut d, a, b"rev".to_vec()).unwrap();
            let mut txn = db.begin().unwrap();
            for k in [b"k1", b"k5", b"k3"] {
                db.put(&mut d, &mut txn, a, k, b"v").unwrap();
            }
            db.commit(txn, &mut d, db.lane(), true).unwrap();
            db.checkpoint().unwrap();
            b = db.make_tree(&mut d, reversed()).unwrap();
            db.order_tree(&mut d, b, b"rev".to_vec()).unwrap();
            let mut txn = db.begin().unwrap();
            for (t, k) in [(a, b"k7"), (b, b"k2"), (b, b"k9"), (b, b"k4")] {
                db.put(&mut d, &mut txn, t, k, b"v").unwrap();
            }
            db.commit(txn, &mut d, db.lane(), true).unwrap();
        }
        let want_a: Vec<Vec<u8>> = [b"k7", b"k5", b"k3", b"k1"]
            .iter()
            .map(|k| k.to_vec())
            .collect();
        let db = Database::open_ordered(disk.clone(), 2_000, 1, cmp, recorded).unwrap();
        assert_eq!(keys(&db, a), want_a);
        let mut d = Descender::new();
        let mut txn = db.begin().unwrap();
        db.put(&mut d, &mut txn, b, b"k6", b"v").unwrap();
        db.commit(txn, &mut d, db.lane(), true).unwrap();
        let want_b: Vec<Vec<u8>> = [b"k9", b"k6", b"k4", b"k2"]
            .iter()
            .map(|k| k.to_vec())
            .collect();
        assert_eq!(keys(&db, b), want_b);
        drop(db);
        disk.power_cut().unwrap();
        let db = Database::open_ordered(disk.clone(), 2_000, 1, cmp, recorded).unwrap();
        assert_eq!(keys(&db, a), want_a);
        assert_eq!(keys(&db, b), want_b);
        db.checkpoint().unwrap();
        drop(db);
        assert!(matches!(
            Database::open(disk, 2_000, 1, cmp),
            Err(LimboError::Corrupt(_))
        ));
    }

    /// Do: for many crash points, a workload that makes trees, writes long and short rows to them
    /// durably, drops the oldest now and then, and checkpoints, on a disk that crashes after that
    /// many operations; cut the power and reopen. Expect: the trees and their rows exactly as
    /// after a prefix of the steps, at least as long as the last acknowledged (AC-NS.2.5: a crash at
    /// each write of a tree made or dropped).
    #[test]
    fn ns2_5_a_crash_at_each_write_of_trees_made_and_dropped() {
        let mut checked = 0;
        for crash in (3..600).step_by(11) {
            let disk = Arc::new(Simulated::new(crash, 1).unwrap());
            let mut states = vec![vec![None; 38]];
            let mut acknowledged = 1usize;
            {
                let db = Database::open(disk.clone(), 2_000, 1, cmp).unwrap();
                disk.crash_after(crash);
                let mut d = Descender::new();
                let mut live: Vec<u64> = Vec::new();
                let mut state: BTreeMap<u64, BTreeMap<Vec<u8>, Vec<u8>>> = BTreeMap::new();
                let snapshot = |state: &BTreeMap<u64, BTreeMap<Vec<u8>, Vec<u8>>>| {
                    (2..40u64)
                        .map(|t| state.get(&t).cloned())
                        .collect::<Vec<_>>()
                };
                'run: for i in 0..18u64 {
                    let Ok(t) = db.make_tree(&mut d, cmp()) else {
                        // Its record may have reached the disk: the tree, empty, under the next id.
                        let mut s = state.clone();
                        s.insert(i + 2, BTreeMap::new());
                        states.push(snapshot(&s));
                        break 'run;
                    };
                    assert_eq!(t, i + 2);
                    live.push(t);
                    state.insert(t, BTreeMap::new());
                    states.push(snapshot(&state));
                    acknowledged = states.len();
                    let mut txn = db.begin().unwrap();
                    let len = if i % 2 == 0 { 2 * PAGE_SIZE + 5 } else { 9 };
                    let v = vec![u8::try_from(i).unwrap(); len];
                    if db.put(&mut d, &mut txn, t, b"row", &v).is_err() {
                        break 'run;
                    }
                    if db.commit(txn, &mut d, db.lane(), true).is_err() {
                        states.push({
                            let mut s = state.clone();
                            s.entry(t).or_default().insert(b"row".to_vec(), v.clone());
                            snapshot(&s)
                        });
                        break 'run;
                    }
                    state.entry(t).or_default().insert(b"row".to_vec(), v);
                    states.push(snapshot(&state));
                    acknowledged = states.len();
                    if i % 3 == 2 {
                        let old = live.remove(0);
                        if db.drop_tree(&mut d, old).is_err() {
                            let mut s = state.clone();
                            s.remove(&old);
                            states.push(snapshot(&s));
                            break 'run;
                        }
                        state.remove(&old);
                        states.push(snapshot(&state));
                        acknowledged = states.len();
                    }
                    if i % 4 == 3 && db.checkpoint().is_err() {
                        break 'run;
                    }
                }
            }
            disk.power_cut().unwrap();
            let db = Database::open(disk.clone(), 2_000, 1, cmp)
                .unwrap_or_else(|e| panic!("crash {crash}: reopen: {e}"));
            let got = made_trees(&db, 40);
            let n = states
                .iter()
                .rposition(|s| s == &got)
                .unwrap_or_else(|| panic!("crash {crash}: trees match no prefix of the steps"));
            assert!(
                n + 1 >= acknowledged,
                "crash {crash}: step {n} recovered, {acknowledged} acknowledged"
            );
            checked += 1;
        }
        assert!(checked > 40);
    }

    /// Do: keep a column copy's feed of tree 0, then commit a value past a page, a short one over
    /// it, and a long one again. Expect: the feed queues each value whole, not its cell's part,
    /// and nothing of the overflow tree (AC-NS.2.4, G11 decision 1 part 2).
    #[test]
    fn ns2_4_a_column_feed_gets_large_values_whole() {
        let db = Database::open(Arc::new(Simulated::new(24, 1).unwrap()), 2_000, 1, cmp).unwrap();
        let feed = Arc::new(Feed::new(0, usize::MAX, Box::new(|| 0)));
        db.keep_columns(feed.clone()).unwrap();
        let mut d = Descender::new();
        let key = b"k".to_vec();
        let mut queued = 0;
        for v in [
            vec![5u8; 3 * PAGE_SIZE],
            vec![6u8; 3],
            vec![7u8; 2 * PAGE_SIZE + 1],
        ] {
            let mut t = db.begin().unwrap();
            db.put(&mut d, &mut t, 0, &key, &v).unwrap();
            db.commit(t, &mut d, db.lane(), false).unwrap();
            queued += key.len() + v.len();
            assert_eq!(feed.queued_bytes(), queued);
        }
        let ops = vec![
            Op::Put {
                tree: 0,
                key: key.clone(),
                value: vec![1; overflow::inline_len(&key).unwrap()],
            },
            Op::Put {
                tree: db.overflow,
                key: overflow::chunk_key(0, &key, 0),
                value: vec![2; 9],
            },
            Op::Put {
                tree: db.overflow,
                key: overflow::chunk_key(0, &key, 1),
                value: vec![3; 4],
            },
            Op::Remove {
                tree: db.overflow,
                key: overflow::chunk_key(0, &key, 2),
            },
        ];
        let whole = db.whole(&ops).unwrap();
        assert_eq!(whole.len(), 1);
        let Op::Put { value, .. } = &whole[0] else {
            panic!("a put")
        };
        assert_eq!(value.len(), overflow::inline_len(&key).unwrap() + 13);
    }

    /// The overflow tree's rows `snap` sees.
    fn chunk_rows(db: &Database, snap: &Snapshot) -> usize {
        let mut d = Descender::new();
        db.v.tree(db.overflow)
            .unwrap()
            .scan(&db.v.pool, &mut d, &db.v.clock, snap, None, usize::MAX)
            .unwrap()
            .len()
    }

    /// Values of every length that matters under `key`: short, one short of the cell's room, the
    /// room exactly, one past it, and several pages; each filled with `fill`.
    fn lengths(key: &[u8], fill: u8) -> Vec<Vec<u8>> {
        let room = overflow::inline_len(key).unwrap();
        let per = overflow::chunk_len(key).unwrap();
        [
            5,
            room - 1,
            room,
            room + 1,
            room + per,
            5 * PAGE_SIZE + 3,
            20_000,
        ]
        .into_iter()
        .map(|n| (0..n).map(|i| fill.wrapping_add(i as u8)).collect())
        .collect()
    }

    /// Do: commit a row's value at every length that matters, from a few bytes to several pages
    /// and back, keeping a snapshot open after each commit; read each snapshot by get and scan.
    /// Expect: each snapshot reads the whole value committed before it; once the row is short
    /// again and then removed, the newest snapshot sees no chunk rows (AC-NS.2.4).
    #[test]
    fn ns2_4_values_past_a_page_read_at_every_snapshot() {
        let db = Database::open(Arc::new(Simulated::new(21, 1).unwrap()), 2_000, 1, cmp).unwrap();
        let mut d = Descender::new();
        let key = b"big".to_vec();
        let mut values = lengths(&key, 1);
        values.extend(lengths(&key, 101).into_iter().rev());
        let mut seen = Vec::new();
        for v in &values {
            let mut t = db.begin().unwrap();
            db.put(&mut d, &mut t, 0, &key, v).unwrap();
            db.put(&mut d, &mut t, 0, b"next", b"n").unwrap();
            db.commit(t, &mut d, db.lane(), false).unwrap();
            seen.push((db.snapshot().unwrap(), v.clone()));
        }
        for (snap, v) in &seen {
            assert_eq!(db.get(&mut d, snap, 0, &key).unwrap().as_ref(), Some(v));
            let all = db.scan(&mut d, snap, 0, None, usize::MAX).unwrap();
            assert_eq!(all.len(), 2);
            assert_eq!((&all[0].0, &all[0].1), (&key, v));
        }
        let last = seen.last().unwrap();
        assert_eq!(
            chunk_rows(&db, &last.0),
            0,
            "a short value leaves no chunks"
        );
        let long = &seen[5];
        assert!(chunk_rows(&db, &long.0) > 2);
        let mut t = db.begin().unwrap();
        db.put(&mut d, &mut t, 0, &key, &long.1).unwrap();
        assert_eq!(
            db.remove(&mut d, &mut t, 0, &key).unwrap(),
            Some(long.1.clone())
        );
        db.commit(t, &mut d, db.lane(), false).unwrap();
        let snap = db.snapshot().unwrap();
        assert_eq!(db.get(&mut d, &snap, 0, &key).unwrap(), None);
        assert_eq!(chunk_rows(&db, &snap), 0, "a removal removes its chunks");
        assert!(db.get(&mut d, &snap, db.overflow, b"x").is_err());
    }

    /// Do: commit a long value; in a transaction replace it with a longer one, take a savepoint,
    /// replace it with a short one and then a different long one, roll back to the savepoint;
    /// then replace it again and abort. Expect: the transaction reads the longer value after the
    /// rollback to its savepoint, and every snapshot after the abort the first long value, its
    /// chunks as many as before (AC-NS.2.4).
    #[test]
    fn ns2_4_rollback_and_savepoints_across_large_values() {
        let db = Database::open(Arc::new(Simulated::new(22, 1).unwrap()), 2_000, 1, cmp).unwrap();
        let mut d = Descender::new();
        let key = b"k".to_vec();
        let vals = lengths(&key, 7);
        let (first, longer, other) = (&vals[4], &vals[6], &vals[5]);
        let mut t = db.begin().unwrap();
        db.put(&mut d, &mut t, 0, &key, first).unwrap();
        db.commit(t, &mut d, db.lane(), false).unwrap();
        let before = chunk_rows(&db, &db.snapshot().unwrap());
        let mut t = db.begin().unwrap();
        assert_eq!(
            db.put(&mut d, &mut t, 0, &key, longer).unwrap().as_ref(),
            Some(first)
        );
        let sp = t.savepoint();
        assert_eq!(
            db.put(&mut d, &mut t, 0, &key, b"tiny").unwrap().as_ref(),
            Some(longer)
        );
        assert_eq!(
            db.put(&mut d, &mut t, 0, &key, other).unwrap().as_deref(),
            Some(&b"tiny"[..])
        );
        db.rollback_to(&mut d, &mut t, sp).unwrap();
        let mine = t.snapshot();
        assert_eq!(
            db.get(&mut d, &mine, 0, &key).unwrap().as_ref(),
            Some(longer)
        );
        db.put(&mut d, &mut t, 0, &key, other).unwrap();
        db.abort(t, &mut d).unwrap();
        let snap = db.snapshot().unwrap();
        assert_eq!(
            db.get(&mut d, &snap, 0, &key).unwrap().as_ref(),
            Some(first)
        );
        assert_eq!(chunk_rows(&db, &snap), before);
    }

    /// Do: commit long values durably, checkpoint with a transaction open that replaced one with
    /// another long value, commit more long values durably, then cut the power and reopen.
    /// Expect: every committed value whole, the open transaction's absent (AC-NS.2.4, T-T1.3.1).
    #[test]
    fn ns2_4_large_values_survive_recovery() {
        let disk = Arc::new(Simulated::new(23, 1).unwrap());
        let mut want = BTreeMap::new();
        {
            let db = Database::open(disk.clone(), 2_000, 1, cmp).unwrap();
            let mut d = Descender::new();
            for (i, v) in lengths(b"a", 3).into_iter().enumerate() {
                let mut t = db.begin().unwrap();
                let k = vec![b'a', i as u8];
                db.put(&mut d, &mut t, 0, &k, &v).unwrap();
                db.commit(t, &mut d, db.lane(), true).unwrap();
                want.insert(k, v);
            }
            let mut open = db.begin().unwrap();
            db.put(&mut d, &mut open, 0, &[b'a', 5], &lengths(b"a", 9)[6])
                .unwrap();
            db.checkpoint().unwrap();
            for (i, v) in lengths(b"b", 4).into_iter().enumerate() {
                let mut t = db.begin().unwrap();
                let k = vec![b'b', i as u8];
                db.put(&mut d, &mut t, 0, &k, &v).unwrap();
                db.commit(t, &mut d, db.lane(), true).unwrap();
                want.insert(k, v);
            }
        }
        disk.power_cut().unwrap();
        let db = Database::open(disk, 2_000, 1, cmp).unwrap();
        assert_eq!(rows(&db), want);
    }
}
