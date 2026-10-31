//! Amphora: the versioned storage's redo log (Amphora's `docs/design/engine.md`, decision 6).
//!
//! A commit appends one record of its row changes, in commit-timestamp order (appended while the
//! clock's publish lock is held), to the pending bytes of the log file now current; whoever next
//! needs commits durable takes the flush lock, writes everything pending in one write, flushes
//! once, and so makes every commit appended before it durable: a group commit with no timer. Two
//! files take turns: a checkpoint switches appends to the other file at its timestamp T, so the
//! file it switched from holds only commits at or before T and is cut once the checkpoint is
//! durable.
//!
//! A record: `magic` (`u32`), the body's length (`u32`), the body's XXH3 (`u64`), then the body:
//! the commit timestamp (`u64`), the change count (`u32`), and each change: the tree (`u64`), its
//! kind (`u8`: 1 a put, 2 a removal), the key's length (`u32`) and key, and a put's value's length
//! (`u32`) and value. Recovery reads records until the first that is torn or doesn't check out:
//! a flush that never ended may leave a prefix of what it wrote.

use super::disk::{Disk, File};
use crate::error::LimboError;
use crate::sync::Mutex;
use crate::Result;

const MAGIC: u32 = 0x4f44_5241;
const HEAD: usize = 16;

/// A row change, as a commit's record carries it and recovery replays it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Op {
    Put {
        tree: u64,
        key: Vec<u8>,
        value: Vec<u8>,
    },
    Remove {
        tree: u64,
        key: Vec<u8>,
    },
}

/// A commit as the log holds it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Record {
    pub ts: u64,
    pub ops: Vec<Op>,
}

fn push_u32(out: &mut Vec<u8>, v: usize) -> Result<()> {
    out.extend_from_slice(
        &u32::try_from(v)
            .map_err(|_| LimboError::TooBig)?
            .to_le_bytes(),
    );
    Ok(())
}

/// The record's bytes.
pub fn encode(ts: u64, ops: &[Op]) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    body.extend_from_slice(&ts.to_le_bytes());
    push_u32(&mut body, ops.len())?;
    for op in ops {
        match op {
            Op::Put { tree, key, value } => {
                body.extend_from_slice(&tree.to_le_bytes());
                body.push(1);
                push_u32(&mut body, key.len())?;
                body.extend_from_slice(key);
                push_u32(&mut body, value.len())?;
                body.extend_from_slice(value);
            }
            Op::Remove { tree, key } => {
                body.extend_from_slice(&tree.to_le_bytes());
                body.push(2);
                push_u32(&mut body, key.len())?;
                body.extend_from_slice(key);
            }
        }
    }
    let mut out = Vec::with_capacity(body.len().saturating_add(HEAD));
    out.extend_from_slice(&MAGIC.to_le_bytes());
    push_u32(&mut out, body.len())?;
    out.extend_from_slice(&twox_hash::XxHash3_64::oneshot(&body).to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Reads fixed-width fields off the front of a buffer, None once it runs out.
struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.rest.len() < n {
            return None;
        }
        let (head, rest) = self.rest.split_at(n);
        self.rest = rest;
        Some(head)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).and_then(|b| b.first().copied())
    }

    fn u32(&mut self) -> Option<usize> {
        let b: [u8; 4] = self.take(4)?.try_into().ok()?;
        usize::try_from(u32::from_le_bytes(b)).ok()
    }

    fn u64(&mut self) -> Option<u64> {
        let b: [u8; 8] = self.take(8)?.try_into().ok()?;
        Some(u64::from_le_bytes(b))
    }
}

fn decode_body(body: &[u8]) -> Option<Record> {
    let mut r = Reader { rest: body };
    let ts = r.u64()?;
    let n = r.u32()?;
    // Each change is at least 13 bytes: a count past what the body could hold is damage.
    if n > body.len() / 13 {
        return None;
    }
    let mut ops = Vec::with_capacity(n);
    for _ in 0..n {
        let tree = r.u64()?;
        let kind = r.u8()?;
        let klen = r.u32()?;
        let key = r.take(klen)?.to_vec();
        ops.push(match kind {
            1 => {
                let vlen = r.u32()?;
                Op::Put {
                    tree,
                    key,
                    value: r.take(vlen)?.to_vec(),
                }
            }
            2 => Op::Remove { tree, key },
            _ => return None,
        });
    }
    if !r.rest.is_empty() {
        return None;
    }
    Some(Record { ts, ops })
}

/// The records of `bytes`, in order, up to the first torn or damaged one, and the bytes they span.
pub fn decode_all(bytes: &[u8]) -> (Vec<Record>, usize) {
    let mut out = Vec::new();
    let mut r = Reader { rest: bytes };
    let mut valid = 0usize;
    while let (Some(magic), Some(len), Some(hash)) = (r.u32(), r.u32(), r.u64()) {
        if u32::try_from(magic).ok() != Some(MAGIC) {
            break;
        }
        let Some(body) = r.take(len) else {
            break;
        };
        if twox_hash::XxHash3_64::oneshot(body) != hash {
            break;
        }
        match decode_body(body) {
            Some(record) => out.push(record),
            None => break,
        }
        valid = bytes.len().saturating_sub(r.rest.len());
    }
    (out, valid)
}

/// What the log has made durable, and where each file's records end on disk.
struct Flushed {
    /// The newest commit whose record a finished flush covers.
    durable: u64,
    /// Bytes each file holds of records.
    ends: [u64; 2],
}

/// Records appended and not yet flushed, by file, and the file appends go to now.
struct Pending {
    bytes: [Vec<u8>; 2],
    /// The newest commit appended to each file's bytes.
    last: [u64; 2],
    current: usize,
}

/// The redo log over a disk's two redo files.
pub struct Redo {
    pending: Mutex<Pending>,
    flush: Mutex<Flushed>,
    /// A flush failed, of the log or a checkpoint: what it held may be lost, so nothing more is
    /// logged or made durable (as the store's flusher, store.md decision 9).
    failed: crate::sync::atomic::AtomicBool,
}

fn failed() -> LimboError {
    LimboError::InternalError(
        "versioned storage: a flush failed; nothing more is made durable until reopened".into(),
    )
}

fn redo_file(i: usize) -> File {
    if i == 0 {
        File::Redo
    } else {
        File::RedoNext
    }
}

impl Redo {
    /// The log as `disk` holds it: appends go to `current` (0 or 1), after the records each file
    /// already ends with (`ends`, as recovery found them), `durable` the newest commit in either.
    pub fn new(current: usize, ends: [u64; 2], durable: u64) -> Self {
        Self {
            pending: Mutex::new(Pending {
                bytes: [Vec::new(), Vec::new()],
                last: [0, 0],
                current: current.min(1),
            }),
            flush: Mutex::new(Flushed { durable, ends }),
            failed: crate::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Appends the record of commit `ts` (call in timestamp order: under the clock's publish lock).
    pub fn append(&self, ts: u64, ops: &[Op]) -> Result<()> {
        self.usable()?;
        let record = encode(ts, ops)?;
        let mut p = self.pending.lock();
        let c = p.current;
        p.bytes
            .get_mut(c)
            .ok_or_else(|| {
                LimboError::InternalError("versioned storage: a missing redo file".into())
            })?
            .extend_from_slice(&record);
        if let Some(last) = p.last.get_mut(c) {
            *last = ts;
        }
        Ok(())
    }

    /// Switches appends to the other file (call under the clock's publish lock, at a checkpoint's
    /// timestamp): the file switched from, which holds only commits up to now.
    pub fn switch(&self) -> usize {
        let mut p = self.pending.lock();
        let from = p.current;
        p.current = 1usize.saturating_sub(from);
        from
    }

    /// Makes every commit appended so far durable: flushes what is pending, unless a flush that
    /// began after `ts` was appended already did. The newest durable commit.
    pub fn sync(&self, disk: &dyn Disk, ts: u64) -> Result<u64> {
        let mut f = self.flush.lock();
        self.usable()?;
        if f.durable >= ts {
            return Ok(f.durable);
        }
        let (bytes, last, current) = {
            let mut p = self.pending.lock();
            (std::mem::take(&mut p.bytes), p.last, p.current)
        };
        // The file switched from first: its commits are older than the other's.
        let order = [1usize.saturating_sub(current), current];
        for i in order {
            let chunk = bytes.get(i).map(Vec::as_slice).unwrap_or_default();
            if chunk.is_empty() {
                continue;
            }
            let end = f.ends.get(i).copied().unwrap_or(0);
            let written = disk
                .write_at(redo_file(i), end, chunk)
                .and_then(|()| disk.sync(redo_file(i)));
            if let Err(e) = written {
                self.poison();
                return Err(e);
            }
            let n = u64::try_from(chunk.len()).map_err(|_| LimboError::IntegerOverflow)?;
            if let Some(e) = f.ends.get_mut(i) {
                *e = end.checked_add(n).ok_or(LimboError::IntegerOverflow)?;
            }
        }
        f.durable = last.iter().copied().fold(f.durable, u64::max);
        Ok(f.durable)
    }

    /// The newest durable commit.
    pub fn durable(&self) -> u64 {
        self.flush.lock().durable
    }

    /// Refused once a flush has failed.
    pub fn usable(&self) -> Result<()> {
        if self.failed.load(crate::sync::atomic::Ordering::Acquire) {
            Err(failed())
        } else {
            Ok(())
        }
    }

    /// Stops logging and flushing until reopened: a flush something durable depends on failed.
    pub fn poison(&self) {
        self.failed
            .store(true, crate::sync::atomic::Ordering::Release);
    }

    /// Cuts redo file `i` to nothing, with its records not yet flushed: a checkpoint durable on
    /// the data file holds its commits, every one up to `covered`.
    pub fn cut(&self, disk: &dyn Disk, i: usize, covered: u64) -> Result<()> {
        let mut f = self.flush.lock();
        if let Some(bytes) = self.pending.lock().bytes.get_mut(i) {
            bytes.clear();
        }
        disk.truncate(redo_file(i), 0)?;
        disk.sync(redo_file(i))?;
        if let Some(e) = f.ends.get_mut(i) {
            *e = 0;
        }
        f.durable = f.durable.max(covered);
        Ok(())
    }
}

/// Cuts each redo file after its last whole record (`ends`, as [`read`] found them), durably: an
/// append there must not leave a torn record's leftovers after it, to be read back as records.
pub fn trim(disk: &dyn Disk, ends: [u64; 2]) -> Result<()> {
    for (i, end) in ends.into_iter().enumerate() {
        let file = redo_file(i);
        if disk.len(file)? > end {
            disk.truncate(file, end)?;
            disk.sync(file)?;
        }
    }
    Ok(())
}

/// The records each redo file holds, read whole (recovery), and where their valid bytes end.
pub fn read(disk: &dyn Disk) -> Result<([Vec<Record>; 2], [u64; 2])> {
    let mut records: [Vec<Record>; 2] = [Vec::new(), Vec::new()];
    let mut ends = [0u64; 2];
    for i in 0..2usize {
        let file = redo_file(i);
        let len = usize::try_from(disk.len(file)?).map_err(|_| LimboError::IntegerOverflow)?;
        let mut bytes = vec![0u8; len];
        let n = disk.read_at(file, 0, &mut bytes)?;
        bytes.truncate(n);
        let (found, valid) = decode_all(&bytes);
        if let Some(e) = ends.get_mut(i) {
            *e = u64::try_from(valid).map_err(|_| LimboError::IntegerOverflow)?;
        }
        if let Some(slot) = records.get_mut(i) {
            *slot = found;
        }
    }
    Ok((records, ends))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::versioned::disk::Simulated;

    fn ops(n: u8) -> Vec<Op> {
        vec![
            Op::Put {
                tree: 1,
                key: vec![n],
                value: vec![n; 3],
            },
            Op::Remove {
                tree: 2,
                key: vec![n, n],
            },
        ]
    }

    /// Do: encode records and decode them, then a stream cut at every byte and one with a byte
    /// flipped. Expect: the records back; a cut stream yields the records wholly before the cut,
    /// a damaged one those before the damage.
    #[test]
    fn records_decode_up_to_the_first_torn_or_damaged_one() {
        let records: Vec<Vec<u8>> = (1..=5u8)
            .map(|n| encode(u64::from(n), &ops(n)).unwrap())
            .collect();
        let stream: Vec<u8> = records.concat();
        let (all, _) = decode_all(&stream);
        assert_eq!(all.len(), 5);
        assert_eq!(all[2], Record { ts: 3, ops: ops(3) });
        let mut whole = 0;
        let mut at = 0;
        for cut in 0..=stream.len() {
            while whole < records.len() && at + records[whole].len() <= cut {
                at += records[whole].len();
                whole += 1;
            }
            assert_eq!(decode_all(&stream[..cut]).0.len(), whole, "cut at {cut}");
        }
        let mut damaged = stream;
        let third = records[0].len() + records[1].len() + 20;
        damaged[third] ^= 0xff;
        assert_eq!(decode_all(&damaged).0.len(), 2);
    }

    /// A disk that counts its flushes.
    struct Counting {
        inner: Simulated,
        syncs: std::sync::atomic::AtomicU64,
    }

    impl Disk for Counting {
        fn read_at(&self, file: File, offset: u64, buf: &mut [u8]) -> Result<usize> {
            self.inner.read_at(file, offset, buf)
        }
        fn write_at(&self, file: File, offset: u64, data: &[u8]) -> Result<()> {
            self.inner.write_at(file, offset, data)
        }
        fn sync(&self, file: File) -> Result<()> {
            self.syncs
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.sync(file)
        }
        fn len(&self, file: File) -> Result<u64> {
            self.inner.len(file)
        }
        fn truncate(&self, file: File, len: u64) -> Result<()> {
            self.inner.truncate(file, len)
        }
    }

    /// Do: append ten commits, ask for the last durable, then for each of them. Expect: one flush
    /// for all ten, and none for the asks after it (group commit: a flush makes every commit
    /// appended before it durable).
    #[test]
    fn one_flush_makes_every_commit_appended_before_it_durable() {
        let disk = Counting {
            inner: Simulated::new(7),
            syncs: std::sync::atomic::AtomicU64::new(0),
        };
        let redo = Redo::new(0, [0, 0], 0);
        for ts in 1..=10 {
            redo.append(ts, &ops(1)).unwrap();
        }
        assert_eq!(redo.sync(&disk, 10).unwrap(), 10);
        for ts in 1..=10 {
            assert_eq!(redo.sync(&disk, ts).unwrap(), 10);
        }
        assert_eq!(disk.syncs.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(read(&disk).unwrap().0[0].len(), 10);
    }

    /// Do: append commits from 8 threads (each under one lock, as the clock's), each thread then
    /// asking for its commit durable; then switch files, append more, flush and read back.
    /// Expect: every commit durable when its sync returns, and every record read back in its file,
    /// in order.
    #[test]
    fn appended_commits_are_durable_once_synced_and_read_back_in_order() {
        let disk = crate::sync::Arc::new(Simulated::new(3));
        let redo = crate::sync::Arc::new(Redo::new(0, [0, 0], 0));
        let clock = crate::sync::Arc::new(Mutex::new(0u64));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (disk, redo, clock) = (disk.clone(), redo.clone(), clock.clone());
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        let ts = {
                            let mut c = clock.lock();
                            *c += 1;
                            redo.append(*c, &ops(1)).unwrap();
                            *c
                        };
                        assert!(redo.sync(&*disk, ts).unwrap() >= ts);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(redo.durable(), 400);
        let from = redo.switch();
        assert_eq!(from, 0);
        redo.append(401, &ops(2)).unwrap();
        redo.sync(&*disk, 401).unwrap();
        let (records, ends) = read(&*disk).unwrap();
        assert_eq!(
            records[0].iter().map(|r| r.ts).collect::<Vec<_>>(),
            (1..=400).collect::<Vec<_>>()
        );
        assert_eq!(
            records[1].iter().map(|r| r.ts).collect::<Vec<_>>(),
            vec![401]
        );
        assert!(ends[0] > 0 && ends[1] > 0);
        redo.cut(&*disk, 0, 400).unwrap();
        assert!(read(&*disk).unwrap().0[0].is_empty());
    }
}
