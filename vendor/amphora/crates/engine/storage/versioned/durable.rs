//! Amphora: a versioned database on disk (Amphora's `docs/design/engine.md`, decisions 6 to 8).
//!
//! The data file holds a header and the pages after it (page `n` at `n` pages in). A commit is
//! logged to the redo log in timestamp order and made durable by a flush of it ([`Redo`]). A
//! checkpoint stops row changes (each holds the gate shared for its page changes; the checkpoint
//! holds it exclusive while it copies), takes the last commit's timestamp T, switches the redo
//! log's file at it, copies the pages written since the last checkpoint and lists the rows whose
//! copied version isn't their version as of T: changes of transactions still open. The copies, the
//! list and the new header go to the next journal, flushed, then over the data file, flushed; then
//! the redo file switched from, which holds only commits up to T, is cut. Making a database is
//! checkpoint 0. A checkpoint that fails leaves the database making nothing more durable until it
//! is reopened: the pages it took as copied may not have reached the file.
//!
//! Recovery takes the newest complete journal at or after the header's checkpoint and writes its
//! pages and header again (a power cut may keep a checkpoint's header and not every page it wrote),
//! loads the pages, puts the rows of its list back to their versions as of T, replays the redo
//! log's commits after T and clears the versions left on cells: no snapshot outlives the process.
//!
//! Journal and header: `magic` (`u64`), the body's length (`u64`) and XXH3 (`u64`), then the body:
//! the checkpoint's sequence number (`u64`), T (`u64`), the pages handed out (`u64`), the tree count
//! (`u64`) and each tree's root (`u64`); a journal's body then holds its page count (`u64`), each
//! page's number (`u64`) and bytes, and its list as one redo record at T. Journals take turns by
//! sequence number's parity, so a checkpoint never writes over the journal recovery may need.

use super::blink::{Comparator, Descender};
use super::disk::{Disk, File};
use super::page::PAGE_SIZE;
use super::pool::Pool;
use super::redo::{self, Op, Redo};
use super::version::{Snapshot, Ts, Txn, Versioned};
use crate::error::LimboError;
use crate::sync::{Arc, Mutex, RwLock};
use crate::Result;

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

/// Reads a whole file.
fn read_all(disk: &dyn Disk, file: File) -> Result<Vec<u8>> {
    let len = usize::try_from(disk.len(file)?).map_err(|_| LimboError::IntegerOverflow)?;
    let mut bytes = vec![0u8; len];
    let n = disk.read_at(file, 0, &mut bytes)?;
    bytes.truncate(n);
    Ok(bytes)
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

/// The pages written since this was last asked, each with a copy of its bytes (asked while no page
/// can change).
fn copy_dirty(pool: &Pool) -> Result<Vec<(u64, Vec<u8>)>> {
    let mut pages = Vec::new();
    for page in pool.take_dirty()? {
        pages.push((page, pool.read(page)?.to_vec()));
    }
    Ok(pages)
}

/// A journal's contents: its stamp, its pages and its list.
struct Journal {
    stamp: Stamp,
    pages: Vec<(u64, Vec<u8>)>,
    list: Vec<Op>,
}

impl Journal {
    fn encode(&self) -> Result<Vec<u8>> {
        let mut body = Vec::new();
        self.stamp.encode(&mut body)?;
        body.extend_from_slice(
            &u64::try_from(self.pages.len())
                .map_err(|_| LimboError::IntegerOverflow)?
                .to_le_bytes(),
        );
        for (no, bytes) in &self.pages {
            body.extend_from_slice(&no.to_le_bytes());
            body.extend_from_slice(bytes);
        }
        body.extend_from_slice(&redo::encode(self.stamp.ts, &self.list)?);
        seal(JOURNAL_MAGIC, &body)
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let body = unseal(JOURNAL_MAGIC, bytes)?;
        let mut at = 0;
        let stamp = Stamp::decode(body, &mut at)?;
        let n = usize::try_from(take_u64(body, &mut at)?).ok()?;
        if n > body.len() / PAGE_SIZE {
            return None;
        }
        let mut pages = Vec::with_capacity(n);
        for _ in 0..n {
            let no = take_u64(body, &mut at)?;
            let end = at.checked_add(PAGE_SIZE)?;
            pages.push((no, body.get(at..end)?.to_vec()));
            at = end;
        }
        let (records, valid) = redo::decode_all(body.get(at..)?);
        if valid != body.len().saturating_sub(at) || records.len() != 1 {
            return None;
        }
        let list = records.into_iter().next()?.ops;
        Some(Self { stamp, pages, list })
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
}

impl Database {
    /// The database `disk` holds, recovered (or made, with `trees` empty trees, if it holds
    /// none), in a pool of `frames` pages.
    pub fn open(
        disk: Arc<dyn Disk>,
        frames: usize,
        trees: usize,
        cmp: impl Fn() -> Comparator,
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
            let Some(j) = Journal::decode(&read_all(&*disk, file)?) else {
                continue;
            };
            let current = header.as_ref().is_none_or(|h| j.stamp.seq >= h.seq);
            if current && newest.as_ref().is_none_or(|o| j.stamp.seq > o.stamp.seq) {
                newest = Some(j);
            }
        }
        let (records, ends) = redo::read(&*disk)?;
        let Some(journal) = newest else {
            // Making a database flushes its journal before it writes the data file: with neither,
            // and no commit logged, it was never made.
            if header.is_none() && n == 0 && records.iter().all(Vec::is_empty) {
                return Self::create(disk, frames, trees, cmp);
            }
            return Err(corrupt(
                "no complete checkpoint journal at or after the data file's header",
            ));
        };
        Self::write_journal_pages(&*disk, &journal)?;
        redo::trim(&*disk, ends)?;
        let Journal { stamp, list, .. } = journal;
        let pool = Pool::new(frames);
        let mut buf = vec![0u8; PAGE_SIZE];
        for page in 1..=stamp.allocated {
            if disk.read_at(File::Data, page_offset(page)?, &mut buf)? != PAGE_SIZE {
                return Err(corrupt("a page past the data file's end"));
            }
            pool.load(page, &buf)?;
        }
        let v = Versioned::open(pool, &stamp.roots, cmp)?;
        let mut d = Descender::new();
        for op in &list {
            v.tree(Self::tree_of(op))?
                .apply_plain(&v.pool, &mut d, op)?;
        }
        // A checkpoint that switched redo files and never finished leaves commits after T in
        // both: they are replayed by timestamp.
        let mut after: Vec<_> = records
            .into_iter()
            .flatten()
            .filter(|r| r.ts > stamp.ts)
            .collect();
        after.sort_by_key(|r| r.ts);
        let mut newest = stamp.ts;
        for record in &after {
            for op in &record.ops {
                v.tree(Self::tree_of(op))?
                    .apply_plain(&v.pool, &mut d, op)?;
            }
            newest = record.ts;
        }
        v.clear_versions(&mut d)?;
        v.clock.recovered(newest);
        // Each checkpoint switches the file appends go to: after checkpoint `seq`, file `seq % 2`.
        let current = usize::from(!stamp.seq.is_multiple_of(2));
        Ok(Self {
            disk,
            v,
            redo: Redo::new(current, ends, newest),
            gate: RwLock::new(()),
            last: Mutex::new(stamp.seq),
        })
    }

    /// A new database of `trees` empty trees: checkpoint 0.
    fn create(
        disk: Arc<dyn Disk>,
        frames: usize,
        trees: usize,
        cmp: impl Fn() -> Comparator,
    ) -> Result<Self> {
        let v = Versioned::new(frames, trees, cmp)?;
        let journal = Journal {
            stamp: Stamp {
                seq: 0,
                ts: 0,
                allocated: v.pool.allocated(),
                roots: v.roots(),
            },
            pages: copy_dirty(&v.pool)?,
            list: Vec::new(),
        };
        // What the redo files hold is no record (the caller found none) of this database.
        redo::trim(&*disk, [0, 0])?;
        Self::write_journal(&*disk, &journal)?;
        Self::write_journal_pages(&*disk, &journal)?;
        Ok(Self {
            disk,
            v,
            redo: Redo::new(0, [0, 0], 0),
            gate: RwLock::new(()),
            last: Mutex::new(0),
        })
    }

    fn tree_of(op: &Op) -> u64 {
        match op {
            Op::Put { tree, .. } | Op::Remove { tree, .. } => *tree,
        }
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

    /// Writes `j` over its journal file, flushed.
    fn write_journal(disk: &dyn Disk, j: &Journal) -> Result<()> {
        let file = journal_file(j.stamp.seq);
        let bytes = j.encode()?;
        disk.truncate(file, 0)?;
        disk.write_at(file, 0, &bytes)?;
        disk.sync(file)
    }

    /// Writes `j`'s pages and header over the data file, flushed.
    fn write_journal_pages(disk: &dyn Disk, j: &Journal) -> Result<()> {
        for (no, bytes) in &j.pages {
            disk.write_at(File::Data, page_offset(*no)?, bytes)?;
        }
        Self::write_header(disk, &j.stamp)?;
        disk.sync(File::Data)
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

    pub fn get(
        &self,
        d: &mut Descender,
        snap: &Snapshot,
        tree: u64,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        self.v
            .tree(tree)?
            .get(&self.v.pool, d, &self.v.clock, snap, key)
    }

    pub fn scan(
        &self,
        d: &mut Descender,
        snap: &Snapshot,
        tree: u64,
        from: Option<&[u8]>,
        most: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.v
            .tree(tree)?
            .scan(&self.v.pool, d, &self.v.clock, snap, from, most)
    }

    pub fn put(
        &self,
        d: &mut Descender,
        txn: &mut Txn,
        tree: u64,
        key: &[u8],
        value: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        let _changing = self.gate.read();
        self.v
            .tree(tree)?
            .put(&self.v.pool, d, &self.v.clock, txn, key, value)
    }

    pub fn remove(
        &self,
        d: &mut Descender,
        txn: &mut Txn,
        tree: u64,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        let _changing = self.gate.read();
        self.v
            .tree(tree)?
            .remove(&self.v.pool, d, &self.v.clock, txn, key)
    }

    /// Commits `txn`, logged; durable before it returns if `durable`, else once a later
    /// [`Database::sync`] returns. Its timestamp.
    pub fn commit(&self, txn: Txn, d: &mut Descender, durable: bool) -> Result<Ts> {
        // Shared for the publish, which a checkpoint's timestamp must not split, and for a
        // rollback a failed append makes, which changes pages.
        let ts = {
            let _changing = self.gate.read();
            self.v
                .commit_logged(txn, d, |ts, ops| self.redo.append(ts, ops))?
        };
        if durable {
            self.redo.sync(&*self.disk, ts)?;
        }
        Ok(ts)
    }

    pub fn abort(&self, txn: Txn, d: &mut Descender) -> Result<()> {
        let _changing = self.gate.read();
        self.v.abort(txn, d)
    }

    /// Makes every commit so far durable: the newest durable commit.
    pub fn sync(&self) -> Result<Ts> {
        let ts = self.v.clock.at_publish(|ts| ts);
        self.redo.sync(&*self.disk, ts)
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
        let (journal, from) = {
            let _alone = self.gate.write();
            let (ts, from) = self.v.clock.at_publish(|ts| (ts, self.redo.switch()));
            let journal = Journal {
                stamp: Stamp {
                    seq,
                    ts,
                    allocated: self.v.pool.allocated(),
                    roots: self.v.roots(),
                },
                pages: copy_dirty(&self.v.pool)?,
                list: self.v.corrections(ts)?,
            };
            (journal, from)
        };
        Self::write_journal(&*self.disk, &journal)?;
        Self::write_journal_pages(&*self.disk, &journal)?;
        // Commits up to T are in the file now; the redo file switched from holds no others.
        self.redo.cut(&*self.disk, from, journal.stamp.ts)?;
        Ok(journal.stamp.ts)
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
        let disk = Arc::new(Simulated::new(5));
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
                db.commit(t, &mut d, true).unwrap();
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
        db.commit(t, &mut d, false).unwrap();
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
        let disk = Arc::new(Simulated::new(9));
        let db = Database::open(disk.clone(), 2_000, 1, cmp).unwrap();
        let mut d = Descender::new();
        let mut base = db.begin().unwrap();
        db.put(&mut d, &mut base, 0, b"a", b"0").unwrap();
        db.put(&mut d, &mut base, 0, b"b", b"0").unwrap();
        db.commit(base, &mut d, true).unwrap();
        let mut kept = db.begin().unwrap();
        db.put(&mut d, &mut kept, 0, b"a", b"1").unwrap();
        let mut lost = db.begin().unwrap();
        db.put(&mut d, &mut lost, 0, b"b", b"9").unwrap();
        db.put(&mut d, &mut lost, 0, b"c", b"9").unwrap();
        db.checkpoint().unwrap();
        db.commit(kept, &mut d, true).unwrap();
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
        let disk = Arc::new(Simulated::new(11));
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
                db.commit(t, &mut d, true).unwrap();
                db.checkpoint().unwrap();
            }
            assert_eq!(db.v.pool.allocated(), 1, "the rows fit the root leaf");
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
        let disk = Arc::new(Simulated::new(13));
        let mut want = BTreeMap::new();
        {
            let db = Database::open(disk.clone(), 2_000, 1, cmp).unwrap();
            let mut d = Descender::new();
            let mut t = db.begin().unwrap();
            for k in 0..20u8 {
                db.put(&mut d, &mut t, 0, &[k], &[k]).unwrap();
                want.insert(vec![k], vec![k]);
            }
            db.commit(t, &mut d, true).unwrap();
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
        let disk = Arc::new(Simulated::new(17));
        let db = Database::open(disk.clone(), 2_000, 1, cmp).unwrap();
        let mut d = Descender::new();
        let mut t = db.begin().unwrap();
        db.put(&mut d, &mut t, 0, b"a", b"1").unwrap();
        db.commit(t, &mut d, true).unwrap();
        let mut t = db.begin().unwrap();
        db.put(&mut d, &mut t, 0, b"b", b"1").unwrap();
        // The journal's cut, write and flush, then the first page's write: the next fails.
        disk.crash_after(4);
        assert!(db.checkpoint().is_err());
        assert!(db.commit(t, &mut d, false).is_err());
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
        let disk = Arc::new(Simulated::new(19));
        {
            let db = Database::open(disk.clone(), 2_000, 1, cmp).unwrap();
            let mut d = Descender::new();
            let mut t = db.begin().unwrap();
            db.put(&mut d, &mut t, 0, b"a", b"1").unwrap();
            db.commit(t, &mut d, true).unwrap();
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
        let end = disk.len(File::Redo).unwrap();
        disk.write_at(File::Redo, end, &tail).unwrap();
        disk.sync(File::Redo).unwrap();
        {
            let db = Database::open(disk.clone(), 2_000, 1, cmp).unwrap();
            let mut d = Descender::new();
            let mut t = db.begin().unwrap();
            db.put(&mut d, &mut t, 0, b"b", b"2").unwrap();
            assert_eq!(
                db.commit(t, &mut d, true).unwrap(),
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
    /// checkpointing every 20 commits while the rest commit on; cut the power and reopen. Expect:
    /// the rows of every commit up to some timestamp, applied in timestamp order, at least up to
    /// the last commit acknowledged durable (T-T1.3.1).
    #[test]
    fn commits_racing_checkpoints_recover_a_prefix() {
        let disk = Arc::new(Simulated::new(23));
        let db = Arc::new(Database::open(disk.clone(), 20_000, 1, cmp).unwrap());
        let writers: Vec<_> = (0..8u8)
            .map(|w| {
                let db = db.clone();
                std::thread::spawn(move || {
                    let mut d = Descender::new();
                    let mut done = Vec::new();
                    for i in 0..200u32 {
                        let mut t = db.begin().unwrap();
                        let key = vec![w, u8::try_from(i % 16).unwrap()];
                        let value = i.to_be_bytes().to_vec();
                        db.put(&mut d, &mut t, 0, &key, &value).unwrap();
                        let durable = i.is_multiple_of(2);
                        let ts = db.commit(t, &mut d, durable).unwrap();
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
            "{n} recovered, {acknowledged} acknowledged"
        );
    }

    /// Do: for many crash points, run a workload of commits (durable, some not) and checkpoints on
    /// a disk that crashes after that many operations, then cut the power and reopen. Expect: the
    /// rows after reopening are exactly those of a prefix of the commits, at least as long as the
    /// last commit acknowledged durable (T-T1.3.1).
    #[test]
    fn a_crash_at_any_operation_recovers_a_prefix_of_commits() {
        let mut checked = 0;
        for crash in (5..400).step_by(7) {
            let disk = Arc::new(Simulated::new(crash));
            let mut commits: Vec<BTreeMap<Vec<u8>, Option<Vec<u8>>>> = Vec::new();
            let mut acknowledged = 0usize;
            {
                let db = Database::open(disk.clone(), 4_000, 1, cmp).unwrap();
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
                            let v = vec![u8::try_from(i).unwrap(); 30];
                            if db.put(&mut d, &mut t, 0, &k, &v).is_err() {
                                break 'run;
                            }
                            changes.insert(k, Some(v));
                        }
                    }
                    commits.push(changes);
                    match db.commit(t, &mut d, !i.is_multiple_of(3)) {
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
            let db = Database::open(disk.clone(), 4_000, 1, cmp).unwrap();
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
            let n = prefixes
                .iter()
                .position(|p| p == &got)
                .unwrap_or_else(|| panic!("crash {crash}: rows match no prefix of the commits"));
            assert!(
                n >= acknowledged,
                "crash {crash}: {n} commits recovered, {acknowledged} acknowledged"
            );
            checked += 1;
        }
        assert!(checked > 50);
    }
}
