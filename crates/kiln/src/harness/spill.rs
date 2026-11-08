//! Sealed chunks' segments kept on disk rather than in memory (goal 11, plan 01a1a87b2e56 step 5).
//!
//! A session copy's chunks once sealed and packed are kept as their metas and their segment
//! (`super::copies::Chunk`): the texts of 70 events deflated, about 650 bytes an event (research f20).
//! In the lab's realistic profile they were the most of what the server held live (`session_segments`,
//! 30 MB of 165 MB, by jemalloc's heap profile), and a server with many long sessions holds them for
//! every one of them. What a viewer's snapshot needs of them is read when it's made, on the blocking
//! pool (`crate::api`'s `send_snapshot`), and nothing else reads them but rarely: so beyond the newest
//! page of a session's events (`super::copies::EVENTS_PAGE`), which the events API answers from memory,
//! a copy's owner writes them to a file of the copy's own here, and keeps of each where it went
//! ([`Spilled`]).
//!
//! - **Written before it's let go of, never synced.** A segment's resident copy goes only once its bytes
//!   are written: a write the system refuses (a full disk, a failing one) leaves the chunk as it was, in
//!   memory, and says so (`kiln_session_spill_failures_total{op="write"}`). A written segment is read back
//!   from the system's cache by any thread of the process at once, synced or not, and nothing outlives the
//!   process that wrote it: the folder is emptied as the next one starts ([`Dir::open`]). So a sync made
//!   nothing durable that anyone reads, and each spill waited on the disk for one, on the blocking pool's
//!   `fs` share, every copy's at once as a server started (goal 11 audit A3; btrfs on production's host
//!   stalls such waits ~0.6 s). A write lost all the same (the disk failing as the system writes it back)
//!   reads back as what wasn't written, which the check below refuses, and every reader falls back as it
//!   does for any segment its file doesn't give back.
//! - **Checked as it's read.** Each spilled segment keeps the CRC-32 of the bytes written; a read that
//!   doesn't give them back is refused as `InvalidData` (`op="read"`), never taken for the segment, and
//!   the file stays: it's removed only once no chunk refers to it ([`File`]'s drop). A snapshot then reads
//!   those events from the conversation store (`super::registry::Snapshot`), and so does the events API.
//! - **No lock.** A file's writes go where an atomic reservation of its end says (one writer a copy at a
//!   time, its owner's job), and reads at their own places, so any number read beside a write.
//!
//! Every file of a server's is in its spill folder ([`Dir`]), made empty as the server starts (what an
//! earlier process spilled belonged to copies it had).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use bytes::Bytes;

use super::packed::Segment;

/// Spilled segments' bytes still referred to, on disk (not memory).
static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
/// The spill files' bytes, live and dead (a file goes once nothing refers to it).
static FILE_BYTES: AtomicUsize = AtomicUsize::new(0);
/// Segments read back: by an owner or a request on a runtime thread, and on the blocking pool.
static READS_SHARD: AtomicU64 = AtomicU64::new(0);
static READS_POOL: AtomicU64 = AtomicU64::new(0);
static WRITE_FAILURES: AtomicU64 = AtomicU64::new(0);
static READ_FAILURES: AtomicU64 = AtomicU64::new(0);

/// Tests: the spill files' data synced to disk (`File::sync_data`), counted where a sync is made: none (goal 11 audit A3;
/// each spill made one, which [`write`] waited for).
#[cfg(test)]
pub(crate) static SYNCS: AtomicU64 = AtomicU64::new(0);

/// A server's spill folder: where its copies' spill files go.
pub struct Dir {
    path: PathBuf,
    /// Each file a name of its own, however many copies a session has had.
    next: AtomicU64,
}

impl Dir {
    /// The folder at `path`, made, and emptied of what an earlier process spilled there.
    pub fn open(path: &Path) -> io::Result<Arc<Self>> {
        std::fs::create_dir_all(path)?;
        for entry in std::fs::read_dir(path)?.flatten() {
            if entry.path().extension().is_some_and(|e| e == "seg") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        Ok(Arc::new(Self { path: path.to_owned(), next: AtomicU64::new(0) }))
    }

    /// A new file for session `id`'s spilled segments.
    fn create(&self, id: &str) -> io::Result<Arc<File>> {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let hash = id.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3));
        let path = self.path.join(format!("{hash:016x}-{n}.seg"));
        let file = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path)?;
        Ok(Arc::new(File { path, file, end: AtomicU64::new(0), live: AtomicU64::new(0) }))
    }
}

/// One copy's spill file: segments written one after another, each where its [`Spilled`] says. Removed
/// once nothing refers to it.
pub struct File {
    path: PathBuf,
    file: std::fs::File,
    /// Where the next write goes: what's been reserved of it.
    end: AtomicU64,
    /// The bytes of its segments still referred to.
    live: AtomicU64,
}

impl File {
    /// Bytes in it no chunk refers to any more (their chunks let go of).
    fn dead(&self) -> u64 {
        self.end.load(Ordering::Relaxed).saturating_sub(self.live.load(Ordering::Relaxed))
    }
}

impl Drop for File {
    fn drop(&mut self) {
        less(&FILE_BYTES, usize::try_from(self.end.load(Ordering::Relaxed)).unwrap_or(usize::MAX));
        let _ = std::fs::remove_file(&self.path);
        #[cfg(test)]
        if let Ok(mut gone) = GONE.lock() {
            gone.push((self.path.clone(), crate::rt::on_shard()));
        }
    }
}

/// Tests: each spill file let go of, and whether that was on a shard ([`give_way`]).
#[cfg(test)]
pub(crate) static GONE: std::sync::Mutex<Vec<(PathBuf, bool)>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
impl Dir {
    /// A new file for session `id`'s spilled segments, as a spill makes one.
    pub(crate) fn new_file(&self, id: &str) -> io::Result<Arc<File>> {
        self.create(id)
    }
}

#[cfg(test)]
impl File {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

/// `old`, the file a copy spilled to before its spill went to `new`, let go of: where it's the file's last reference, on the
/// blocking pool (`crate::rt::SHARE_FS`), which its going removes and closes (`File`'s drop), so never on its owner's shard,
/// where they held the shard 130 to 250 µs, past its quantum, as "a copy's spill taken in" in a Mac's lab (goal 9, plan
/// 01a1a87b2cec step 3). One its chunks still refer to, or `new` itself, only counts one fewer, here; so does any off a
/// shard, and any the pool won't take (none running, a test's).
pub fn give_way(old: Arc<File>, new: &Arc<File>) {
    if Arc::ptr_eq(&old, new) || Arc::strong_count(&old) > 1 || !crate::rt::on_shard() {
        return;
    }
    let _ = hyper_rt::blocking::run(crate::rt::SHARE_FS, move || drop(old));
}

/// `n` off `count`, never below none.
fn less(count: &AtomicUsize, n: usize) {
    let mut now = count.load(Ordering::Relaxed);
    while let Err(seen) = count.compare_exchange_weak(now, now.saturating_sub(n), Ordering::Relaxed, Ordering::Relaxed) {
        now = seen;
    }
}

/// `n` off `count`, never below none.
fn less64(count: &AtomicU64, n: u64) {
    let mut now = count.load(Ordering::Relaxed);
    while let Err(seen) = count.compare_exchange_weak(now, now.saturating_sub(n), Ordering::Relaxed, Ordering::Relaxed) {
        now = seen;
    }
}

#[cfg(test)]
thread_local! {
    /// Tests: segments read back from a spill file on this thread ([`Spilled::segment`]), as `super::packed::INFLATED`
    /// counts inflates: what a request's polls on its shard read from disk (goal 11 audit A3: none).
    pub(crate) static READ_BACK: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Where a segment went, and what to check it by: shared by every chunk that holds it (a view's copy
/// and the owner's), its bytes counted live until the last lets go.
#[derive(Clone)]
pub struct Spilled(Arc<Placed>);

struct Placed {
    file: Arc<File>,
    at: u64,
    bytes: usize,
    /// CRC-32 of the bytes written.
    sum: u32,
    /// The segment's own CRC-32 and length (what it holds inflated).
    crc: u32,
    len: u64,
}

impl Drop for Placed {
    fn drop(&mut self) {
        less64(&self.file.live, u64::try_from(self.bytes).unwrap_or(u64::MAX));
        less(&LIVE_BYTES, self.bytes);
    }
}

impl Spilled {
    /// What its segment holds inflated, in bytes: its chunk's texts joined, as `super::copies::Chunk::texts_len` reckons them
    /// from the chunk's metas (goal 11, plan 01a1a87b2e56 step 5, the memory audit's row A5: a snapshot's weight, read on its
    /// viewer's shard).
    pub fn texts_len(&self) -> u64 {
        self.0.len
    }

    /// Its bytes on disk.
    pub fn bytes(&self) -> usize {
        self.0.bytes
    }

    /// The segment, read back and checked against what was written: refused (`InvalidData`) when the
    /// disk doesn't give those bytes back. A read on a runtime thread is counted apart (an owner changing
    /// an event spilled; the blocking pool reads them for snapshots).
    pub fn segment(&self) -> io::Result<Segment> {
        let p = &self.0;
        if crate::rt::on_shard() {
            READS_SHARD.fetch_add(1, Ordering::Relaxed);
        } else {
            READS_POOL.fetch_add(1, Ordering::Relaxed);
        }
        #[cfg(test)]
        READ_BACK.with(|n| n.set(n.get().wrapping_add(1)));
        let mut buf = vec![0u8; p.bytes];
        let read = read_at(&p.file.file, &mut buf, p.at).and_then(|()| {
            if crc32fast::hash(&buf) == p.sum { Ok(()) } else { Err(io::Error::new(io::ErrorKind::InvalidData, "a spilled segment didn't read back as written")) }
        });
        if let Err(e) = read {
            READ_FAILURES.fetch_add(1, Ordering::Relaxed);
            return Err(e);
        }
        Ok(Segment { deflated: Bytes::from(buf), crc: p.crc, len: p.len })
    }
}

/// `segments` written to session `id`'s spill file, in the order given: where each went. Not synced (the
/// module's notes: nothing outlives the process, and a lost write is refused as it's read). Into `file`
/// while its dead bytes (its chunks let go of) are fewer than its live ones, else into a new one, which the
/// old gives way to as its last chunks go: a long session's file holds about what's live of it and as much
/// again at the most. Any failure fails the lot (none is taken as spilled): the chunks stay in memory.
pub fn write(dir: &Dir, file: Option<Arc<File>>, id: &str, segments: &[Arc<Segment>]) -> io::Result<(Arc<File>, Vec<Spilled>)> {
    let done = (|| {
        let file = match file.filter(|f| f.dead() <= f.live.load(Ordering::Relaxed)) {
            Some(f) => f,
            None => dir.create(id)?,
        };
        let mut placed = Vec::with_capacity(segments.len());
        for s in segments {
            let n = u64::try_from(s.deflated.len()).map_err(|_| io::Error::other("a segment past a file's size"))?;
            let at = file.end.fetch_add(n, Ordering::Relaxed);
            FILE_BYTES.fetch_add(s.deflated.len(), Ordering::Relaxed);
            write_at(&file.file, &s.deflated, at)?;
            placed.push((at, crc32fast::hash(&s.deflated), s));
        }
        let spilled = placed
            .into_iter()
            .map(|(at, sum, s)| {
                file.live.fetch_add(u64::try_from(s.deflated.len()).unwrap_or(u64::MAX), Ordering::Relaxed);
                LIVE_BYTES.fetch_add(s.deflated.len(), Ordering::Relaxed);
                Spilled(Arc::new(Placed { file: Arc::clone(&file), at, bytes: s.deflated.len(), sum, crc: s.crc, len: s.len }))
            })
            .collect();
        Ok((file, spilled))
    })();
    if done.is_err() {
        WRITE_FAILURES.fetch_add(1, Ordering::Relaxed);
    }
    done
}

#[cfg(unix)]
fn write_at(f: &std::fs::File, buf: &[u8], at: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(f, buf, at)
}

#[cfg(unix)]
fn read_at(f: &std::fs::File, buf: &mut [u8], at: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(f, buf, at)
}

#[cfg(windows)]
fn write_at(f: &std::fs::File, mut buf: &[u8], mut at: u64) -> io::Result<()> {
    while !buf.is_empty() {
        let n = std::os::windows::fs::FileExt::seek_write(f, buf, at)?;
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        buf = buf.get(n..).unwrap_or_default();
        at = at.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
    }
    Ok(())
}

#[cfg(windows)]
fn read_at(f: &std::fs::File, mut buf: &mut [u8], mut at: u64) -> io::Result<()> {
    while !buf.is_empty() {
        let n = std::os::windows::fs::FileExt::seek_read(f, &mut *buf, at)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buf = std::mem::take(&mut buf).get_mut(n..).unwrap_or_default();
        at = at.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn write_at(_: &std::fs::File, _: &[u8], _: u64) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(not(any(unix, windows)))]
fn read_at(_: &std::fs::File, _: &mut [u8], _: u64) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

/// The spill's figures on `/metrics`: its bytes on disk (live, and the files' whole), the segments read
/// back (`on`: a runtime thread or the blocking pool), and the writes and reads that failed.
pub fn render(o: &mut String) {
    use std::fmt::Write as _;
    let _ = writeln!(o, "# HELP kiln_session_spill_bytes Sealed chunks' segments kept on disk rather than in memory: those still referred to (live), and the spill files' bytes (file).\n# TYPE kiln_session_spill_bytes gauge");
    let _ = writeln!(o, "kiln_session_spill_bytes{{kind=\"live\"}} {}", LIVE_BYTES.load(Ordering::Relaxed));
    let _ = writeln!(o, "kiln_session_spill_bytes{{kind=\"file\"}} {}", FILE_BYTES.load(Ordering::Relaxed));
    let _ = writeln!(o, "# HELP kiln_session_spill_reads_total Spilled segments read back, by where: a runtime thread (shard) or the blocking pool (pool).\n# TYPE kiln_session_spill_reads_total counter");
    let _ = writeln!(o, "kiln_session_spill_reads_total{{on=\"shard\"}} {}", READS_SHARD.load(Ordering::Relaxed));
    let _ = writeln!(o, "kiln_session_spill_reads_total{{on=\"pool\"}} {}", READS_POOL.load(Ordering::Relaxed));
    let _ = writeln!(o, "# HELP kiln_session_spill_failures_total Spill writes (the chunks stayed in memory) and reads (refused, never taken for the segment) that failed.\n# TYPE kiln_session_spill_failures_total counter");
    let _ = writeln!(o, "kiln_session_spill_failures_total{{op=\"write\"}} {}", WRITE_FAILURES.load(Ordering::Relaxed));
    let _ = writeln!(o, "kiln_session_spill_failures_total{{op=\"read\"}} {}", READ_FAILURES.load(Ordering::Relaxed));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(text: &str) -> anyhow::Result<Arc<Segment>> {
        Ok(Arc::new(super::super::packed::deflate(flate2::Compression::default(), [text.as_bytes()])?))
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: segments spilled read back as they were; a file whose bytes
    /// changed under it is refused, not taken; and a file goes once nothing refers to it.
    #[test]
    fn g11_s5_spilled_segments_read_back_as_written_or_are_refused() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let dir = Dir::open(&tmp.path().join("spill"))?;
        let (a, b) = (segment(&"{\"id\":\"a\"},".repeat(300))?, segment(&"{\"id\":\"b\"},".repeat(500))?);
        let (file, spilled) = write(&dir, None, "s1", &[Arc::clone(&a), Arc::clone(&b)])?;
        anyhow::ensure!(spilled.len() == 2);
        for (s, want) in spilled.iter().zip([&a, &b]) {
            let got = s.segment()?;
            anyhow::ensure!(got.deflated == want.deflated && got.crc == want.crc && got.len == want.len, "a segment came back changed");
        }
        // The same file takes more while it's live; changed under the chunks, a read is refused.
        let (again, more) = write(&dir, Some(Arc::clone(&file)), "s1", &[segment("{\"id\":\"c\"}")?])?;
        anyhow::ensure!(Arc::ptr_eq(&again, &file), "a live file takes more");
        let path = file.path.clone();
        let mut bytes = std::fs::read(&path)?;
        if let Some(x) = bytes.get_mut(3) {
            *x ^= 0xff;
        }
        std::fs::write(&path, &bytes)?;
        anyhow::ensure!(spilled.first().map(Spilled::segment).is_some_and(|r| r.is_err_and(|e| e.kind() == io::ErrorKind::InvalidData)), "a changed segment is refused");
        // Nothing refers to it: it goes.
        drop((spilled, more, again, file));
        anyhow::ensure!(!path.exists(), "the file went with the last chunk that referred to it");
        Ok(())
    }

    /// Goal 11 audit A3: a spill isn't synced, and needn't be. Do: a spill file an earlier process left; the folder opened,
    /// as a server opens it as it starts; spills of two copies' segments; then their bytes lost under them (one file cut to
    /// nothing, as a write the system never wrote back leaves it; the other's bytes zeroed). Expect: the earlier process's
    /// file gone, so none outlives the process that wrote it; no sync made; each segment read back at once as written; a
    /// lost one refused, never taken for the segment (its readers fall back to the conversation store:
    /// `copies::tests::g11_s5_a_spill_that_doesnt_read_back_is_read_from_the_store`). Each spill synced its file and waited
    /// for the disk: on that code the count is one a spill, and this fails.
    #[test]
    fn g11_a3_a_spill_is_not_synced_and_a_lost_write_is_refused() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let folder = tmp.path().join("spill");
        std::fs::create_dir_all(&folder)?;
        let stale = folder.join("00000000000000ab-3.seg");
        std::fs::write(&stale, b"an earlier process's")?;
        let dir = Dir::open(&folder)?;
        anyhow::ensure!(!stale.exists(), "the folder emptied of what an earlier process spilled");
        let syncs = SYNCS.load(Ordering::SeqCst);
        let (a, b) = (segment(&"{\"id\":\"a\"},".repeat(300))?, segment(&"{\"id\":\"b\"},".repeat(500))?);
        let (file_a, spilled_a) = write(&dir, None, "s1", &[Arc::clone(&a)])?;
        let (file_b, spilled_b) = write(&dir, None, "s2", &[Arc::clone(&b)])?;
        anyhow::ensure!(SYNCS.load(Ordering::SeqCst) == syncs, "a spill made {} syncs", SYNCS.load(Ordering::SeqCst) - syncs);
        for (s, want) in spilled_a.iter().chain(&spilled_b).zip([&a, &b]) {
            let got = s.segment()?;
            anyhow::ensure!(got.deflated == want.deflated && got.crc == want.crc && got.len == want.len, "read back at once, as written");
        }
        // Lost under them: cut to nothing, and zeroed.
        std::fs::OpenOptions::new().write(true).open(&file_a.path)?.set_len(0)?;
        let len = usize::try_from(std::fs::metadata(&file_b.path)?.len())?;
        std::fs::write(&file_b.path, vec![0u8; len])?;
        for (what, s) in [("cut", spilled_a.first()), ("zeroed", spilled_b.first())] {
            let read = s.map(Spilled::segment);
            anyhow::ensure!(read.as_ref().is_some_and(Result::is_err), "a {what} spill refused, not taken for its segment");
        }
        drop((spilled_a, spilled_b, file_a, file_b));
        Ok(())
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: a write the disk refuses fails whole, nothing taken as spilled.
    #[cfg(unix)]
    #[test]
    fn g11_s5_a_refused_write_spills_nothing() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let dir = Dir::open(&tmp.path().join("spill"))?;
        // The folder gone from under it: no file can be made.
        std::fs::remove_dir_all(tmp.path().join("spill"))?;
        anyhow::ensure!(write(&dir, None, "s1", &[segment("{\"id\":\"a\"}")?]).is_err(), "nothing to write into, nothing spilled");
        Ok(())
    }
}
