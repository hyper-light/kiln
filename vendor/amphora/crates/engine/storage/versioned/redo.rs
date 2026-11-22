//! Amphora: the versioned storage's redo log (Amphora's `docs/design/engine.md`, decisions 6 and
//! 10).
//!
//! The log is in lanes, each a pair of files of its own: one lane is one group-committed log;
//! more are per-thread logs (Haubenschild et al., SIGMOD 2020), a writer appending to its own. A
//! commit appends one record of its row changes to its lane's pending bytes, under the clock's
//! publish lock, so commit timestamps run with no gap (every commit takes the next one and logs).
//! Whoever needs a commit durable makes each lane durable through it: a lane holding nothing at or
//! before it is already (no I/O); else it takes the lane's flush lock, writes everything the lane
//! holds pending in one write and flushes it (one submission on io_uring), so one flush serves
//! every commit appended to the lane before it began: a group commit with no timer and no group
//! size, each group what arrived while the flush before it ran. Lanes flush side by side, on the
//! threads that wait for them. A commit is acknowledged once every lane is durable through it, so
//! what a power cut keeps is a prefix of the commits: recovery merges the lanes by timestamp and
//! replays from the checkpoint up to the first timestamp no lane holds, then cuts every lane after
//! the last record it replayed (records past a gap were never acknowledged, and must not be read
//! back beside the commits that take their timestamps next). Two files a lane take turns: a
//! checkpoint switches appends to the other turn at its timestamp T, so the turn it switched from
//! holds only commits at or before T and is cut once the checkpoint is durable.
//!
//! A record: `magic` (`u32`), the body's length (`u32`), the body's XXH3 (`u64`), then the body:
//! the commit timestamp (`u64`), the change count (`u32`), and each change: the tree (`u64`), its
//! kind (`u8`: 1 a put, 2 a removal), the key's length (`u32`) and key, and a put's value's length
//! (`u32`) and value; or a tree made (3) or dropped (4), with an empty key (`AC-NS.2.5`); or a
//! tree's order (5), its encoding in the key's place (`AC-NS.2.1`). Recovery
//! reads records until the first that is torn or doesn't check out:
//! a flush that never ended may leave a prefix of what it wrote.

use super::disk::{Disk, File};
use crate::error::LimboError;
use crate::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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
    /// Tree `tree` made, empty (`AC-NS.2.5`): a record of its own.
    Made {
        tree: u64,
    },
    /// Tree `tree` dropped (`AC-NS.2.5`): a record of its own.
    Dropped {
        tree: u64,
    },
    /// Tree `tree`'s keys ordered as `order` says (G7 step 2, `AC-NS.2.1`): a record of its own,
    /// durable before a row of the tree is written, or a checkpoint's list naming it, so recovery
    /// orders the tree before it replays a row of it. `order` is the opener's encoding.
    Ordered {
        tree: u64,
        order: Vec<u8>,
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
            Op::Made { tree } | Op::Dropped { tree } => {
                body.extend_from_slice(&tree.to_le_bytes());
                body.push(if matches!(op, Op::Made { .. }) { 3 } else { 4 });
                push_u32(&mut body, 0)?;
            }
            Op::Ordered { tree, order } => {
                body.extend_from_slice(&tree.to_le_bytes());
                body.push(5);
                push_u32(&mut body, order.len())?;
                body.extend_from_slice(order);
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
            3 if key.is_empty() => Op::Made { tree },
            4 if key.is_empty() => Op::Dropped { tree },
            5 => Op::Ordered { tree, order: key },
            _ => return None,
        });
    }
    if !r.rest.is_empty() {
        return None;
    }
    Some(Record { ts, ops })
}

/// The records of `bytes`, in order, up to the first torn or damaged one, each with the offset
/// its bytes end at.
fn decode_spans(bytes: &[u8]) -> Vec<(Record, usize)> {
    let mut out = Vec::new();
    let mut r = Reader { rest: bytes };
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
            Some(record) => out.push((record, bytes.len().saturating_sub(r.rest.len()))),
            None => break,
        }
    }
    out
}

/// The records of `bytes`, in order, up to the first torn or damaged one, and the bytes they span.
pub fn decode_all(bytes: &[u8]) -> (Vec<Record>, usize) {
    let spans = decode_spans(bytes);
    let valid = spans.last().map_or(0, |(_, end)| *end);
    (spans.into_iter().map(|(r, _)| r).collect(), valid)
}

/// A lane's records appended and not yet flushed, and how many, by turn.
struct Pending {
    bytes: [Vec<u8>; 2],
    records: [u64; 2],
}

/// What the log's flushes have done: their count and the records they carried. Records over
/// flushes is the group size, which no constant sets: each flush carries what arrived while the
/// one before it ran, so it follows the device's flush time as the log measures it, flush by flush.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Counters {
    pub flushes: u64,
    pub records: u64,
}

/// A lane: its pending records, where its files' records end (held for a flush), and the newest
/// commit through which its every record is durable.
struct Lane {
    pending: Mutex<Pending>,
    ends: Mutex<[u64; 2]>,
    durable: AtomicU64,
}

/// A writer's lane, as [`Redo::lane`] hands them out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LaneId(usize);

/// The redo log over a disk's lanes.
pub struct Redo {
    lanes: Vec<Lane>,
    /// The turn appends go to; switched by a checkpoint, under the clock's publish lock as appends.
    turn: AtomicUsize,
    /// The newest commit appended to any lane: set after its record is in its lane's pending bytes,
    /// so a flush that takes a lane's bytes takes every record of the lane up to it.
    appended: AtomicU64,
    /// Lanes handed to writers so far, round the lanes.
    handed: AtomicUsize,
    flushes: AtomicU64,
    flushed: AtomicU64,
    /// A flush failed, of the log or a checkpoint: what it held may be lost, so nothing more is
    /// logged or made durable (as the store's flusher, store.md decision 9).
    failed: AtomicBool,
}

fn failed() -> LimboError {
    LimboError::InternalError(
        "versioned storage: a flush failed; nothing more is made durable until reopened".into(),
    )
}

fn missing() -> LimboError {
    LimboError::InternalError("versioned storage: a missing redo lane".into())
}

impl Redo {
    /// The log as recovery left it: `ends[lane][turn]` where each file's records end, appends going
    /// to `turn`, every commit through `durable` durable.
    pub fn new(ends: Vec<[u64; 2]>, turn: usize, durable: u64) -> Result<Self> {
        if ends.is_empty() {
            return Err(LimboError::InvalidArgument(
                "versioned storage: a redo log needs a lane".into(),
            ));
        }
        Ok(Self {
            lanes: ends
                .into_iter()
                .map(|e| Lane {
                    pending: Mutex::new(Pending {
                        bytes: [Vec::new(), Vec::new()],
                        records: [0, 0],
                    }),
                    ends: Mutex::new(e),
                    durable: AtomicU64::new(durable),
                })
                .collect(),
            turn: AtomicUsize::new(turn.min(1)),
            appended: AtomicU64::new(durable),
            handed: AtomicUsize::new(0),
            flushes: AtomicU64::new(0),
            flushed: AtomicU64::new(0),
            failed: AtomicBool::new(false),
        })
    }

    /// A lane for a writer: each in turn, round the lanes (one each while there are as many lanes
    /// as writers).
    pub fn lane(&self) -> LaneId {
        let n = self.handed.fetch_add(1, Ordering::Relaxed);
        LaneId(n.checked_rem(self.lanes.len()).unwrap_or(0))
    }

    fn get(&self, lane: usize) -> Result<&Lane> {
        self.lanes.get(lane).ok_or_else(missing)
    }

    /// Appends the record of commit `ts` to `lane` (call in timestamp order, every timestamp: under
    /// the clock's publish lock).
    pub fn append(&self, lane: LaneId, ts: u64, ops: &[Op]) -> Result<()> {
        self.usable()?;
        let record = encode(ts, ops)?;
        let turn = self.turn.load(Ordering::Acquire);
        {
            let mut p = self.get(lane.0)?.pending.lock();
            p.bytes
                .get_mut(turn)
                .ok_or_else(missing)?
                .extend_from_slice(&record);
            if let Some(n) = p.records.get_mut(turn) {
                *n = n.saturating_add(1);
            }
        }
        self.appended.store(ts, Ordering::Release);
        Ok(())
    }

    /// Switches appends to the other turn (call under the clock's publish lock, at a checkpoint's
    /// timestamp): the turn switched from, which holds only commits up to now.
    pub fn switch(&self) -> usize {
        let from = self.turn.load(Ordering::Acquire);
        self.turn
            .store(1usize.saturating_sub(from), Ordering::Release);
        from
    }

    /// Makes every commit through `ts` durable, `lane`'s first (where its writer's record is),
    /// then any other lane holding one not yet durable.
    pub fn sync(&self, disk: &dyn Disk, lane: LaneId, ts: u64) -> Result<()> {
        self.usable()?;
        self.flush_through(disk, lane.0, ts)?;
        self.sync_all(disk, ts)
    }

    /// Makes every commit through `ts` durable, lane by lane.
    pub fn sync_all(&self, disk: &dyn Disk, ts: u64) -> Result<()> {
        self.usable()?;
        for i in 0..self.lanes.len() {
            self.flush_through(disk, i, ts)?;
        }
        Ok(())
    }

    /// Makes lane `i` durable through `ts`: nothing to do if a flush did already; else, under its
    /// flush lock, what it holds pending written and flushed (none, if it holds nothing).
    fn flush_through(&self, disk: &dyn Disk, i: usize, ts: u64) -> Result<()> {
        let lane = self.get(i)?;
        if lane.durable.load(Ordering::Acquire) >= ts {
            return Ok(());
        }
        let mut ends = lane.ends.lock();
        self.usable()?;
        if lane.durable.load(Ordering::Acquire) >= ts {
            return Ok(());
        }
        let (bytes, records, through, turn) = {
            let mut p = lane.pending.lock();
            // Read under the lane's lock: every record of the lane up to it is in what is taken.
            let through = self.appended.load(Ordering::Acquire);
            (
                std::mem::take(&mut p.bytes),
                std::mem::take(&mut p.records)
                    .iter()
                    .fold(0u64, |a, n| a.saturating_add(*n)),
                through,
                self.turn.load(Ordering::Acquire),
            )
        };
        // The turn switched from first: its commits are older than the other's.
        for t in [1usize.saturating_sub(turn), turn] {
            let chunk = bytes.get(t).map(Vec::as_slice).unwrap_or_default();
            if chunk.is_empty() {
                continue;
            }
            let end = ends.get(t).copied().unwrap_or(0);
            if let Err(e) = disk.write_sync(File::Redo { lane: i, turn: t }, end, chunk) {
                self.poison();
                return Err(e);
            }
            let n = u64::try_from(chunk.len()).map_err(|_| LimboError::IntegerOverflow)?;
            if let Some(e) = ends.get_mut(t) {
                *e = end.checked_add(n).ok_or(LimboError::IntegerOverflow)?;
            }
        }
        if records > 0 {
            self.flushes.fetch_add(1, Ordering::Relaxed);
            self.flushed.fetch_add(records, Ordering::Relaxed);
        }
        lane.durable.fetch_max(through, Ordering::AcqRel);
        Ok(())
    }

    /// The flushes so far and the records they carried.
    pub fn counters(&self) -> Counters {
        Counters {
            flushes: self.flushes.load(Ordering::Relaxed),
            records: self.flushed.load(Ordering::Relaxed),
        }
    }

    /// The newest commit through which every lane is durable.
    pub fn durable(&self) -> u64 {
        self.lanes
            .iter()
            .map(|l| l.durable.load(Ordering::Acquire))
            .min()
            .unwrap_or(0)
    }

    /// Refused once a flush has failed.
    pub fn usable(&self) -> Result<()> {
        if self.failed.load(Ordering::Acquire) {
            Err(failed())
        } else {
            Ok(())
        }
    }

    /// Stops logging and flushing until reopened: a flush something durable depends on failed.
    pub fn poison(&self) {
        self.failed.store(true, Ordering::Release);
    }

    /// Cuts every lane's file of turn `turn` to nothing, with its records not yet flushed: a
    /// checkpoint durable on the data file holds its commits, every one up to `covered`.
    pub fn cut(&self, disk: &dyn Disk, turn: usize, covered: u64) -> Result<()> {
        for (i, lane) in self.lanes.iter().enumerate() {
            let mut ends = lane.ends.lock();
            {
                let mut p = lane.pending.lock();
                if let Some(bytes) = p.bytes.get_mut(turn) {
                    bytes.clear();
                }
                if let Some(n) = p.records.get_mut(turn) {
                    *n = 0;
                }
            }
            let file = File::Redo { lane: i, turn };
            disk.truncate(file, 0)?;
            disk.sync(file)?;
            if let Some(e) = ends.get_mut(turn) {
                *e = 0;
            }
            lane.durable.fetch_max(covered, Ordering::AcqRel);
        }
        Ok(())
    }
}

/// A redo file's records as recovery read them, each with the offset its bytes end at.
pub struct Found {
    pub lane: usize,
    pub turn: usize,
    records: Vec<(Record, u64)>,
}

impl Found {
    /// It holds no record.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

/// Every lane's files, read and decoded side by side (SiloR's parallel recovery: Zheng, Tu,
/// Liskov, Madden, OSDI 2014), as many at once as the machine runs threads.
pub fn read(disk: &dyn Disk) -> Result<Vec<Found>> {
    let files: Vec<(usize, usize)> = (0..disk.lanes())
        .flat_map(|lane| [(lane, 0), (lane, 1)])
        .collect();
    let one = |(lane, turn): (usize, usize)| -> Result<Found> {
        let file = File::Redo { lane, turn };
        let len = usize::try_from(disk.len(file)?).map_err(|_| LimboError::IntegerOverflow)?;
        let mut bytes = vec![0u8; len];
        let n = disk.read_at(file, 0, &mut bytes)?;
        bytes.truncate(n);
        let mut records = Vec::new();
        for (record, end) in decode_spans(&bytes) {
            records.push((
                record,
                u64::try_from(end).map_err(|_| LimboError::IntegerOverflow)?,
            ));
        }
        Ok(Found {
            lane,
            turn,
            records,
        })
    };
    let next = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .min(files.len());
    let mut found = Vec::with_capacity(files.len());
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| -> Result<Vec<Found>> {
                    let mut mine = Vec::new();
                    while let Some(&f) = files.get(next.fetch_add(1, Ordering::Relaxed)) {
                        mine.push(one(f)?);
                    }
                    Ok(mine)
                })
            })
            .collect();
        for w in workers {
            let mine = w.join().map_err(|_| {
                LimboError::InternalError("versioned storage: a recovery reader ended".into())
            })??;
            found.extend(mine);
        }
        Ok::<(), LimboError>(())
    })?;
    Ok(found)
}

/// What recovery replays after a checkpoint, and where each redo file is cut.
pub struct Replay {
    /// The commits after the checkpoint, in timestamp order, with no gap.
    pub records: Vec<Record>,
    /// The last commit replayed (the checkpoint's, with none after it).
    pub newest: u64,
    /// `ends[lane][turn]`: where each file's records end once cut after the last it replayed.
    pub ends: Vec<[u64; 2]>,
}

/// The commit-order merge: the lanes' records after checkpoint `t`, by timestamp, up to the first
/// timestamp none holds. A file whose records aren't in timestamp order, or two records of one
/// timestamp after `t`, are damage no append makes.
pub fn merge(found: Vec<Found>, lanes: usize, t: u64) -> Result<Replay> {
    let mut after: Vec<(u64, usize)> = Vec::new();
    for (i, f) in found.iter().enumerate() {
        let mut last = 0u64;
        for (record, _) in &f.records {
            if record.ts <= last {
                return Err(LimboError::Corrupt(format!(
                    "versioned storage: redo lane {} turn {} holds commit {} after {last}",
                    f.lane, f.turn, record.ts
                )));
            }
            last = record.ts;
            if record.ts > t {
                after.push((record.ts, i));
            }
        }
    }
    after.sort_unstable();
    let mut newest = t;
    for (ts, _) in &after {
        if *ts == newest {
            return Err(LimboError::Corrupt(format!(
                "versioned storage: two redo records of commit {ts}"
            )));
        }
        if Some(*ts) != newest.checked_add(1) {
            break;
        }
        newest = *ts;
    }
    let mut ends = vec![[0u64; 2]; lanes];
    let mut records = Vec::new();
    for f in found {
        let mut keep = 0u64;
        for (record, end) in f.records {
            if record.ts > newest {
                break;
            }
            keep = end;
            if record.ts > t {
                records.push(record);
            }
        }
        if let Some(e) = ends.get_mut(f.lane).and_then(|e| e.get_mut(f.turn)) {
            *e = keep;
        }
    }
    records.sort_unstable_by_key(|r| r.ts);
    Ok(Replay {
        records,
        newest,
        ends,
    })
}

/// Cuts each redo file after the last record recovery replayed (`ends`, as [`merge`] found them),
/// durably: an append there must not leave a torn record's leftovers, or a record past a gap, after
/// it, to be read back as records.
pub fn trim(disk: &dyn Disk, ends: &[[u64; 2]]) -> Result<()> {
    for (lane, turns) in ends.iter().enumerate() {
        for (turn, end) in turns.iter().enumerate() {
            let file = File::Redo { lane, turn };
            if disk.len(file)? > *end {
                disk.truncate(file, *end)?;
                disk.sync(file)?;
            }
        }
    }
    Ok(())
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
            Op::Made { tree: u64::from(n) },
            Op::Dropped { tree: 3 },
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
        fn lanes(&self) -> usize {
            self.inner.lanes()
        }
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

    fn counting(lanes: usize) -> Counting {
        Counting {
            inner: Simulated::new(7, lanes).unwrap(),
            syncs: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// The timestamps lane `lane`'s file of turn `turn` holds.
    fn held(disk: &dyn Disk, lane: usize, turn: usize) -> Vec<u64> {
        read(disk)
            .unwrap()
            .into_iter()
            .find(|f| f.lane == lane && f.turn == turn)
            .map(|f| f.records.iter().map(|(r, _)| r.ts).collect())
            .unwrap_or_default()
    }

    /// Do: append ten commits to one lane, ask for the last durable, then for each of them.
    /// Expect: one flush for all ten, and none for the asks after it (group commit: a flush makes
    /// every commit appended before it durable).
    #[test]
    fn one_flush_makes_every_commit_appended_before_it_durable() {
        let disk = counting(1);
        let redo = Redo::new(vec![[0, 0]], 0, 0).unwrap();
        let lane = redo.lane();
        for ts in 1..=10 {
            redo.append(lane, ts, &ops(1)).unwrap();
        }
        redo.sync(&disk, lane, 10).unwrap();
        assert_eq!(redo.durable(), 10);
        for ts in 1..=10 {
            redo.sync(&disk, lane, ts).unwrap();
        }
        assert_eq!(disk.syncs.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(
            redo.counters(),
            Counters {
                flushes: 1,
                records: 10
            }
        );
        assert_eq!(held(&disk, 0, 0), (1..=10).collect::<Vec<_>>());
    }

    /// Do: on three lanes, commit 1 to lane 0 and 2 to lane 1, then ask for commit 2 durable on
    /// lane 1. Expect: two flushes, lane 1's and lane 0's (commit 1 is before it), none of lane 2,
    /// which holds nothing, and every lane durable through 2.
    #[test]
    fn a_commit_is_durable_once_every_lane_is_through_it() {
        let disk = counting(3);
        let redo = Redo::new(vec![[0, 0]; 3], 0, 0).unwrap();
        let lanes: Vec<_> = (0..3).map(|_| redo.lane()).collect();
        redo.append(lanes[0], 1, &ops(1)).unwrap();
        redo.append(lanes[1], 2, &ops(2)).unwrap();
        redo.sync(&disk, lanes[1], 2).unwrap();
        assert_eq!(disk.syncs.load(std::sync::atomic::Ordering::Relaxed), 2);
        assert_eq!(redo.durable(), 2);
        assert_eq!(
            (held(&disk, 0, 0), held(&disk, 1, 0), held(&disk, 2, 0)),
            (vec![1], vec![2], vec![])
        );
    }

    /// Do: commits from 8 threads on 4 lanes (each under one lock, as the clock's), each thread
    /// then asking for its commit durable; then switch turns, append more, flush, merge and cut.
    /// Expect: every commit durable when its sync returns; the merge replays all 401 in order; the
    /// turn switched from cut to nothing.
    #[test]
    fn appended_commits_are_durable_once_synced_and_merged_in_order() {
        let disk = crate::sync::Arc::new(Simulated::new(3, 4).unwrap());
        let redo = crate::sync::Arc::new(Redo::new(vec![[0, 0]; 4], 0, 0).unwrap());
        let clock = crate::sync::Arc::new(Mutex::new(0u64));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (disk, redo, clock) = (disk.clone(), redo.clone(), clock.clone());
                std::thread::spawn(move || {
                    let lane = redo.lane();
                    for _ in 0..50 {
                        let ts = {
                            let mut c = clock.lock();
                            *c += 1;
                            redo.append(lane, *c, &ops(1)).unwrap();
                            *c
                        };
                        redo.sync(&*disk, lane, ts).unwrap();
                        assert!(redo.durable() >= ts);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(redo.durable(), 400);
        assert_eq!(redo.switch(), 0);
        let lane = redo.lane();
        redo.append(lane, 401, &ops(2)).unwrap();
        redo.sync(&*disk, lane, 401).unwrap();
        let replay = merge(read(&*disk).unwrap(), 4, 0).unwrap();
        assert_eq!(replay.newest, 401);
        assert_eq!(
            replay.records.iter().map(|r| r.ts).collect::<Vec<_>>(),
            (1..=401).collect::<Vec<_>>()
        );
        redo.cut(&*disk, 0, 400).unwrap();
        for l in 0..4 {
            assert!(held(&*disk, l, 0).is_empty());
        }
        assert_eq!(merge(read(&*disk).unwrap(), 4, 400).unwrap().newest, 401);
    }

    fn write(disk: &dyn Disk, lane: usize, tss: &[u64]) {
        let file = File::Redo { lane, turn: 0 };
        let mut at = disk.len(file).unwrap();
        for &ts in tss {
            let r = encode(ts, &ops(1)).unwrap();
            disk.write_sync(file, at, &r).unwrap();
            at += u64::try_from(r.len()).unwrap();
        }
    }

    /// Do: lane 0 holds commits 1, 3, 5 and 6, lane 1 commit 2 (4 was lost); merge after 0, cut
    /// the lanes where it says and read them back; then merge after 4. Expect: commits 1 to 3
    /// replayed, lane 0 cut after 3 and lane 1 after 2, so 5 and 6 (never acknowledged: 4 was
    /// not durable) can't be read back beside the commits that take their timestamps next; after
    /// 4 (a checkpoint covering it), 5 and 6.
    #[test]
    fn the_merge_replays_up_to_the_first_missing_commit_and_cuts_each_lane_there() {
        let disk = Simulated::new(5, 2).unwrap();
        write(&disk, 0, &[1, 3, 5, 6]);
        write(&disk, 1, &[2]);
        let replay = merge(read(&disk).unwrap(), 2, 0).unwrap();
        assert_eq!(replay.newest, 3);
        assert_eq!(
            replay.records.iter().map(|r| r.ts).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(merge(read(&disk).unwrap(), 2, 4).unwrap().newest, 6);
        trim(&disk, &replay.ends).unwrap();
        assert_eq!(
            (held(&disk, 0, 0), held(&disk, 1, 0)),
            (vec![1, 3], vec![2])
        );
    }

    /// Do: a lane whose records go back in time, and two lanes holding one commit. Expect: each
    /// refused as damage (`Corrupt`), never replayed.
    #[test]
    fn records_out_of_order_or_twice_are_refused() {
        let disk = Simulated::new(5, 2).unwrap();
        write(&disk, 0, &[1, 3, 2]);
        assert!(matches!(
            merge(read(&disk).unwrap(), 2, 0),
            Err(LimboError::Corrupt(_))
        ));
        let disk = Simulated::new(5, 2).unwrap();
        write(&disk, 0, &[1, 2]);
        write(&disk, 1, &[2]);
        assert!(matches!(
            merge(read(&disk).unwrap(), 2, 0),
            Err(LimboError::Corrupt(_))
        ));
    }
}
