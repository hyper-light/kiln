//! kiln's log lines, written by a thread of their own (goal 9, plan 01a1a87b2cec step 3: nothing on a shard waits on a
//! lock or a blocking call; goal 5's lockless hot path). `tracing_subscriber`'s fmt layer wrote each line from the
//! thread that logged it, under stdout's lock and in a `write(2)` of its own: a shard that logged ("took a custody
//! record", for each runner's record; "reach identity", for each runner's hello) waited for every other thread's line
//! and for whoever reads kiln's output, journald behind a disk that stalls among them.
//!
//! Here the line the layer formats, its format and destination as before (stdout: journald on bare metal, the file a
//! unit sends it to), is pushed onto a lock-free queue ([`concurrent_queue`]) and the thread writes it. Pushing takes
//! no lock and makes no call: the writer, asleep, is woken once for each line that finds it so ([`Thread::unpark`]),
//! never by a line that finds it writing. A thread's lines keep their order; across threads, their timestamps say it.
//! A line past the queue's bound ([`LINES`], [`LINES_BYTES`]) is let go and counted (`kiln_log_lines_dropped_total`),
//! and the writer says how many as it next writes. As the process ends ([`Writing`]) the writer writes all the queue
//! holds before it stops, and a line made after that is written where it's made; a panicking thread first waits for
//! the lines before its panic's to be written ([`PANIC_FLUSH`]), then writes its own, so a crash's last lines aren't
//! left in a queue.

use std::io::Write as _;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering, fence};
use std::thread::Thread;
use std::time::{Duration, Instant};

use concurrent_queue::{ConcurrentQueue, PopError, PushError};

use crate::memory::Owner;

/// Lines the queue holds at the most: journald's burst, the lines it takes from a service in one of its rate limit's
/// intervals (`RateLimitBurst=10000` in each `RateLimitIntervalSec=30s`, journald.conf(5)'s defaults). Past that
/// journald drops a service's lines itself, so a queue holding more would hold only lines it drops.
pub const LINES: usize = 10_000;

/// The bytes the queue holds at the most: as much as observe's node keeps waiting of a source's log before it takes
/// no more (`crate::observe::ingest::WAITING_LOG_BYTES`), kiln's own log being one more source of lines waiting to be
/// written.
pub const LINES_BYTES: usize = crate::observe::ingest::WAITING_LOG_BYTES;

/// How long a panicking thread waits for the lines before its panic's to be written, at the most: as long as kiln's
/// supervisor gives a server it stops (`supervise`'s grace, 20 s), the longest kiln waits for a process's last words.
/// A writer that keeps up takes microseconds; one behind a stalled reader holds the panic no longer than this.
pub const PANIC_FLUSH: Duration = Duration::from_secs(20);

/// The queue of lines and what the writer thread knows of it.
pub struct Queue {
    lines: ConcurrentQueue<Vec<u8>>,
    /// The lines it holds and their bytes, against its bounds.
    held: AtomicUsize,
    held_bytes: AtomicUsize,
    /// Lines let go at a bound since the writer last said so, and ever.
    dropped: AtomicU64,
    dropped_total: AtomicU64,
    /// Lines queued ever, and written ever: what a flush waits for.
    queued: AtomicU64,
    written: AtomicU64,
    /// The writer, and whether it's asleep (to be woken by the next line).
    writer: OnceLock<Thread>,
    asleep: AtomicBool,
    owner: &'static Owner,
}

impl Queue {
    fn new(owner: &'static Owner) -> Self {
        owner.bound_at(LINES_BYTES);
        Self {
            lines: ConcurrentQueue::unbounded(),
            held: AtomicUsize::new(0),
            held_bytes: AtomicUsize::new(0),
            dropped: AtomicU64::new(0),
            dropped_total: AtomicU64::new(0),
            queued: AtomicU64::new(0),
            written: AtomicU64::new(0),
            writer: OnceLock::new(),
            asleep: AtomicBool::new(false),
            owner,
        }
    }

    /// Queues `line`, or lets it go at a bound, counted: `Err` with the line once the queue is closed, for the
    /// caller to write itself. No lock, no call but the writer's wake when it's asleep.
    fn put(&self, line: Vec<u8>) -> Result<(), Vec<u8>> {
        let n = line.len();
        let lines = self.held.fetch_add(1, Ordering::AcqRel);
        let bytes = self.held_bytes.fetch_add(n, Ordering::AcqRel);
        if lines >= LINES || bytes.saturating_add(n) > LINES_BYTES {
            self.held.fetch_sub(1, Ordering::AcqRel);
            self.held_bytes.fetch_sub(n, Ordering::AcqRel);
            if self.lines.is_closed() {
                return Err(line);
            }
            self.dropped.fetch_add(1, Ordering::Relaxed);
            self.dropped_total.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        if let Err(PushError::Closed(line) | PushError::Full(line)) = self.lines.push(line) {
            self.held.fetch_sub(1, Ordering::AcqRel);
            self.held_bytes.fetch_sub(n, Ordering::AcqRel);
            return Err(line);
        }
        self.owner.add(n);
        self.queued.fetch_add(1, Ordering::AcqRel);
        // The line queued before the writer's sleep is read: it either finds the line or is woken (`write_to`).
        fence(Ordering::SeqCst);
        if self.asleep.swap(false, Ordering::SeqCst)
            && let Some(t) = self.writer.get()
        {
            t.unpark();
        }
        Ok(())
    }

    /// The writer thread's loop: each line to `out` as it comes, and after it how many were let go since it last
    /// said, if any; asleep while there's none; until the queue is closed and all it held is written.
    fn write_to(&self, out: &mut impl std::io::Write) {
        loop {
            match self.lines.pop() {
                Ok(line) => {
                    let n = line.len();
                    let _ = out.write_all(&line);
                    let dropped = self.dropped.swap(0, Ordering::Relaxed);
                    if dropped > 0 {
                        let _ = writeln!(out, "kiln: {dropped} log lines let go: the log's writer was behind (its queue holds {LINES} lines, {LINES_BYTES} bytes)");
                    }
                    let _ = out.flush();
                    self.held.fetch_sub(1, Ordering::AcqRel);
                    self.held_bytes.fetch_sub(n, Ordering::AcqRel);
                    self.owner.sub(n);
                    self.written.fetch_add(1, Ordering::AcqRel);
                }
                Err(PopError::Closed) => return,
                Err(PopError::Empty) => {
                    // Asleep only once no line came after it said so: a line that finds it asleep wakes it.
                    self.asleep.store(true, Ordering::SeqCst);
                    fence(Ordering::SeqCst);
                    if self.lines.is_empty() && !self.lines.is_closed() {
                        std::thread::park();
                    }
                    self.asleep.store(false, Ordering::SeqCst);
                }
            }
        }
    }

    /// Closes the queue: what it holds is still written, and a line after it is its maker's to write.
    fn close(&self) {
        self.lines.close();
        if let Some(t) = self.writer.get() {
            t.unpark();
        }
    }

    /// Waits until every line queued before it has been written, `within` at the most: whether they were.
    fn flush(&self, within: Duration) -> bool {
        let target = self.queued.load(Ordering::Acquire);
        if let Some(t) = self.writer.get() {
            t.unpark();
        }
        let deadline = Instant::now() + within;
        while self.written.load(Ordering::Acquire) < target {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::yield_now();
        }
        true
    }
}

/// The process's queue, once [`start`] has made it.
static LOG: OnceLock<Queue> = OnceLock::new();

/// Where the fmt layer writes its lines ([`tracing_subscriber::fmt::MakeWriter`]): each line to the queue, or, with no
/// queue (or a closed one), where it's made.
#[derive(Clone, Copy, Debug, Default)]
pub struct Lines;

/// Held for as long as the process logs: dropped, it closes the queue and waits for the writer thread to write all
/// it holds.
pub struct Writing {
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Writing {
    fn drop(&mut self) {
        if let Some(q) = LOG.get() {
            q.close();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The log's lines written to stdout by a thread of their own, once a process: the writer for the fmt layer, and
/// what keeps the thread going. With no thread to be had (or a second call), lines are written where they're made.
pub fn start() -> (Lines, Writing) {
    let fresh = LOG.get().is_none();
    let q = LOG.get_or_init(|| Queue::new(&crate::memory::owners::LOG_LINES));
    if !fresh {
        return (Lines, Writing { thread: None });
    }
    // The writer names itself before it can sleep, so a line that finds it asleep can always wake it.
    let thread = std::thread::Builder::new()
        .name("kiln-log".into())
        .spawn(move || {
            let _ = q.writer.set(std::thread::current());
            q.write_to(&mut std::io::stdout());
        })
        .ok();
    if thread.is_none() {
        q.close();
    }
    (Lines, Writing { thread })
}

/// Lines let go at the queue's bound since the process started (`kiln_log_lines_dropped_total`).
pub fn dropped_total() -> u64 {
    LOG.get().map_or(0, |q| q.dropped_total.load(Ordering::Relaxed))
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Lines {
    type Writer = Line;

    fn make_writer(&'a self) -> Self::Writer {
        Line { bytes: Vec::new(), to: LOG.get() }
    }
}

/// One line as the fmt layer writes it, handed to the queue as it's dropped.
pub struct Line {
    bytes: Vec<u8>,
    to: Option<&'static Queue>,
}

impl std::io::Write for Line {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.bytes.extend_from_slice(b);
        Ok(b.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for Line {
    fn drop(&mut self) {
        let bytes = std::mem::take(&mut self.bytes);
        if bytes.is_empty() {
            return;
        }
        let Some(q) = self.to else { return write_here(&bytes) };
        // A panic's word: after the lines before it, and at once, since the process may end before the thread writes it.
        if std::thread::panicking() {
            let _ = q.flush(PANIC_FLUSH);
            return write_here(&bytes);
        }
        if let Err(bytes) = q.put(bytes) {
            write_here(&bytes);
        }
    }
}

/// A line written to stdout by the thread that made it.
fn write_here(bytes: &[u8]) {
    let _ = std::io::stdout().lock().write_all(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal 9, plan 01a1a87b2cec step 3: a line is queued, not written by the thread that made it; the writer writes
    /// the lines in their order and says once how many were let go at the bound, which `/metrics` counts; and once
    /// the queue is closed the writer ends with all it held written, a line after that given back to its maker.
    #[test]
    fn g9_s3_log_lines_are_queued_written_in_order_and_dropped_counted() {
        let q = Queue::new(&crate::memory::owners::LOG_LINES);
        for word in ["one\n", "two\n"] {
            assert!(q.put(word.as_bytes().to_vec()).is_ok());
        }
        assert_eq!((q.lines.len(), q.held.load(Ordering::Relaxed)), (2, 2), "both queued, none written");
        // Past the bytes the queue holds: let go and counted.
        assert!(q.put(vec![b'x'; LINES_BYTES]).is_ok());
        assert_eq!((q.lines.len(), q.dropped.load(Ordering::Relaxed), q.dropped_total.load(Ordering::Relaxed)), (2, 1, 1));
        q.close();
        assert_eq!(q.put(b"late\n".to_vec()), Err(b"late\n".to_vec()), "closed: its maker writes it");
        let mut out = Vec::new();
        q.write_to(&mut out);
        let text = String::from_utf8_lossy(&out).into_owned();
        assert!(text.starts_with("one\nkiln: 1 log lines let go"), "{text}");
        assert!(text.ends_with("two\n"), "{text}");
        assert_eq!((q.dropped.load(Ordering::Relaxed), q.dropped_total.load(Ordering::Relaxed)), (0, 1), "said once, counted for good");
        assert_eq!((q.held.load(Ordering::Relaxed), q.held_bytes.load(Ordering::Relaxed), q.written.load(Ordering::Relaxed)), (0, 0, 2));
    }

    /// Goal 9, plan 01a1a87b2cec step 3: past its bound in lines, journald's burst, a line is let go and counted too.
    #[test]
    fn g9_s3_the_queue_holds_journalds_burst_of_lines() {
        let q = Queue::new(&crate::memory::owners::LOG_LINES);
        for _ in 0..LINES {
            assert!(q.put(b"l\n".to_vec()).is_ok());
        }
        assert!(q.put(b"one more\n".to_vec()).is_ok());
        assert_eq!((q.lines.len(), q.dropped_total.load(Ordering::Relaxed)), (LINES, 1));
    }

    /// Goal 9, plan 01a1a87b2cec step 3: the writer thread, asleep, wakes for a line; a flush waits for every line
    /// queued before it to be written; and closing ends the thread with the queue written.
    #[test]
    fn g9_s3_the_writer_wakes_for_a_line_and_a_flush_waits_for_it() -> anyhow::Result<()> {
        let q: &'static Queue = Box::leak(Box::new(Queue::new(&crate::memory::owners::LOG_LINES)));
        let out = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        struct Shared(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Shared {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().map_err(|_| std::io::Error::other("poisoned"))?.extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut shared = Shared(std::sync::Arc::clone(&out));
        let t = std::thread::spawn(move || {
            let _ = q.writer.set(std::thread::current());
            q.write_to(&mut shared);
        });
        // Let it fall asleep, then wake it with a line.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !q.asleep.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(q.put(b"woken\n".to_vec()).is_ok());
        assert!(q.flush(Duration::from_secs(10)), "the line was written");
        assert_eq!(out.lock().map(|o| o.clone()).unwrap_or_default(), b"woken\n".to_vec());
        assert!(q.put(b"last\n".to_vec()).is_ok());
        q.close();
        t.join().map_err(|_| anyhow::anyhow!("the writer panicked"))?;
        assert_eq!(out.lock().map(|o| o.clone()).unwrap_or_default(), b"woken\nlast\n".to_vec(), "what it held is written as it closes");
        Ok(())
    }
}
