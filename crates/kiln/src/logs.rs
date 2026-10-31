//! Job logs: one append-only file per step at `<data>/logs/<id[..2]>/<job>/<step>.log`, plus a
//! broadcast channel per live job so any number of UI viewers can tail without touching disk.
//! A per-job byte cap protects the disk from runaway output. Beside each step's log, `<step>.times`
//! notes when each piece of it arrived (its byte offset and unix ms, a line each), for the times
//! Ship's Output puts beside its lines (plan 01a1856fe81375b1bd1d1d86f270a45b, step 9). And
//! `<step>.digests` keeps each chunk the leg sent by its length and digest, with where it's kept
//! (or `-` past the cap), a retry's mark between attempts, so what kiln keeps is checked against
//! the chain the leg signed, a capped log included (docs/ci.md W27-R05, [`kiln_ci::logs`]).

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use parking_lot::Mutex;

#[derive(Debug, Clone)]
pub enum LogEvent {
    Data { idx: u32, data: Bytes },
    Step { idx: u32, status: &'static str },
    End,
}

struct Live {
    tx: async_broadcast::Sender<LogEvent>,
    /// Keeps the channel open while nobody is watching.
    _idle: async_broadcast::InactiveReceiver<LogEvent>,
    files: HashMap<u32, std::fs::File>,
    /// Each step's `<step>.times`, open while it's written.
    times: HashMap<u32, std::fs::File>,
    /// Each step's `<step>.digests`, open while it's written.
    digests: HashMap<u32, std::fs::File>,
    bytes: u64,
    truncated: bool,
}

#[derive(Clone)]
pub struct Logs {
    dir: PathBuf,
    max_bytes: u64,
    live: Arc<Mutex<HashMap<String, Live>>>,
}

fn safe(id: &str) -> String {
    id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect()
}

impl Logs {
    pub fn new(dir: PathBuf, max_bytes: u64) -> Self {
        Self { dir, max_bytes, live: Arc::new(Mutex::new(HashMap::new())) }
    }

    fn job_dir(&self, job: &str) -> PathBuf {
        let j = safe(job);
        let shard = j.get(j.len().saturating_sub(2)..).unwrap_or("00").to_owned();
        self.dir.join(shard).join(j)
    }

    fn step_path(&self, job: &str, idx: u32) -> PathBuf {
        self.job_dir(job).join(format!("{idx}.log"))
    }

    fn times_path(&self, job: &str, idx: u32) -> PathBuf {
        self.job_dir(job).join(format!("{idx}.times"))
    }

    fn digests_path(&self, job: &str, idx: u32) -> PathBuf {
        self.job_dir(job).join(format!("{idx}.digests"))
    }

    fn with_live<R>(&self, job: &str, f: impl FnOnce(&mut Live) -> R) -> R {
        let mut g = self.live.lock();
        let live = g.entry(job.to_owned()).or_insert_with(|| {
            let (mut tx, rx) = async_broadcast::broadcast(1024);
            // Slow viewers lose the oldest chunks (and are told so) instead of stalling writers.
            tx.set_overflow(true);
            tx.set_await_active(false);
            let idle = rx.deactivate();
            Live { tx, _idle: idle, files: HashMap::new(), times: HashMap::new(), digests: HashMap::new(), bytes: 0, truncated: false }
        });
        f(live)
    }

    /// Append output for a step. Never fails the caller: disk errors are logged and the data still
    /// reaches live viewers.
    pub fn append(&self, job: &str, idx: u32, data: &[u8]) {
        self.put(job, idx, data, true);
    }

    /// Output kiln copies into a step's log from elsewhere (a reused step's, from its source leg,
    /// W24-R07): kept and shown as [`Logs::append`] keeps it, and noted as no chunk of the leg's,
    /// which it wasn't taken from, so the leg's log checks against what the leg itself sent
    /// (docs/ci.md W27-R05).
    pub fn append_copy(&self, job: &str, idx: u32, data: &[u8]) {
        self.put(job, idx, data, false);
    }

    /// `data` appended to step `idx`'s log, and noted as a chunk its leg sent when it's `taken`.
    fn put(&self, job: &str, idx: u32, data: &[u8], taken: bool) {
        crate::metrics::global().log_bytes.add(data.len() as u64);
        let path = self.step_path(job, idx);
        let times_path = self.times_path(job, idx);
        let digests_path = self.digests_path(job, idx);
        let max = self.max_bytes;
        let notice: &[u8] = b"\r\n\x1b[33m[kiln] log size limit reached; further output discarded\x1b[0m\r\n";
        // The chunk as the leg sent it, kept by its digest whether its bytes are or not.
        let (len, digest) = (data.len() as u64, kiln_ci::logs::digest(data));
        self.with_live(job, |live| {
            if live.truncated {
                if taken {
                    note_piece(live, idx, &digests_path, &kiln_ci::logs::Piece { at: None, len, digest });
                }
                return;
            }
            let (chunk, truncate_now) = if live.bytes + data.len() as u64 > max {
                (notice, true)
            } else {
                (data, false)
            };
            live.bytes += chunk.len() as u64;
            live.truncated = truncate_now;
            let file = match live.files.entry(idx) {
                std::collections::hash_map::Entry::Occupied(o) => Some(o.into_mut()),
                std::collections::hash_map::Entry::Vacant(v) => {
                    let opened = path
                        .parent()
                        .map_or(Ok(()), std::fs::create_dir_all)
                        .and_then(|()| {
                            std::fs::OpenOptions::new().create(true).append(true).open(&path)
                        });
                    match opened {
                        Ok(f) => Some(v.insert(f)),
                        Err(e) => {
                            tracing::warn!(error = %e, path = %path.display(), "cannot open log");
                            None
                        }
                    }
                }
            };
            // Where this piece starts in the step's log (whatever else wrote to it, a retry's
            // mark), noted with when it came once it's written.
            let at = file.as_ref().and_then(|f| f.metadata().ok()).map(|m| m.len());
            let wrote = match file {
                Some(f) => match f.write_all(chunk) {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::warn!(error = %e, "log write failed");
                        false
                    }
                },
                None => false,
            };
            // The leg's chunk: where it's kept, or that it isn't (the cap's notice took its place).
            let kept = at.filter(|_| wrote && !truncate_now);
            if taken {
                note_piece(live, idx, &digests_path, &kiln_ci::logs::Piece { at: kept, len, digest });
            }
            // A piece that holds no bytes holds no line: nothing to note.
            if let (true, Some(at), false) = (wrote, at, chunk.is_empty()) {
                let times = match live.times.entry(idx) {
                    std::collections::hash_map::Entry::Occupied(o) => Some(o.into_mut()),
                    std::collections::hash_map::Entry::Vacant(v) => match std::fs::OpenOptions::new().create(true).append(true).open(&times_path) {
                        Ok(f) => Some(v.insert(f)),
                        Err(e) => {
                            tracing::warn!(error = %e, path = %times_path.display(), "cannot open log times");
                            None
                        }
                    },
                };
                if let Some(t) = times
                    && let Err(e) = writeln!(t, "{at} {}", crate::util::now_ms())
                {
                    tracing::warn!(error = %e, "log times write failed");
                }
            }
            let _ = live.tx.try_broadcast(LogEvent::Data { idx, data: Bytes::copy_from_slice(chunk) });
        });
    }

    pub fn step(&self, job: &str, idx: u32, status: &'static str) {
        self.with_live(job, |live| {
            if status != "running" {
                live.files.remove(&idx);
                live.times.remove(&idx);
                live.digests.remove(&idx);
            }
            let _ = live.tx.try_broadcast(LogEvent::Step { idx, status });
        });
    }

    /// The job is over: close files, tell viewers, drop the channel.
    pub fn finish(&self, job: &str) {
        if let Some(live) = self.live.lock().remove(job) {
            let _ = live.tx.try_broadcast(LogEvent::End);
        }
    }

    /// Subscribe to a job's live output. Subscribe *before* reading the backlog so nothing falls
    /// in between (viewers may see a few bytes twice at the seam, never miss any).
    pub fn subscribe(&self, job: &str) -> async_broadcast::Receiver<LogEvent> {
        self.with_live(job, |live| live.tx.new_receiver())
    }

    pub fn is_live(&self, job: &str) -> bool {
        self.live.lock().contains_key(job)
    }

    pub async fn read(&self, job: &str, idx: u32) -> Vec<u8> {
        let p = self.step_path(job, idx);
        crate::rt::blocking(move || std::fs::read(p).unwrap_or_default()).await.unwrap_or_default()
    }

    /// When each piece of a step's log arrived, oldest first: its byte offset in the log and unix
    /// ms. A log written before kiln noted them has none.
    pub async fn times(&self, job: &str, idx: u32) -> Vec<(u64, i64)> {
        let p = self.times_path(job, idx);
        crate::rt::blocking(move || std::fs::read_to_string(p).map(|s| times_of(&s)).unwrap_or_default()).await.unwrap_or_default()
    }

    /// Each attempt's pieces of a step's log, oldest first, as `<step>.digests` keeps them: a log
    /// written before kiln kept them has one attempt of none.
    pub async fn pieces(&self, job: &str, idx: u32) -> Vec<Vec<kiln_ci::logs::Piece>> {
        let p = self.digests_path(job, idx);
        crate::rt::blocking(move || std::fs::read_to_string(p).map(|s| kiln_ci::logs::parse(&s)).unwrap_or_else(|_| kiln_ci::logs::parse("")))
            .await
            .unwrap_or_else(|_| kiln_ci::logs::parse(""))
    }

    /// The steps of `job` whose chunks kiln keeps by digest (a `<step>.digests` beside each log), in
    /// order: each step its leg sent a log of since kiln kept digests.
    pub async fn kept_steps(&self, job: &str) -> Vec<u32> {
        let dir = self.job_dir(job);
        crate::rt::blocking(move || {
            let mut steps: Vec<u32> = std::fs::read_dir(&dir).into_iter().flatten().flatten().filter_map(|e| e.file_name().to_str()?.strip_suffix(".digests")?.parse().ok()).collect();
            steps.sort_unstable();
            steps
        })
        .await
        .unwrap_or_default()
    }

    /// A step's log as kiln keeps it against `signed`, the chain its leg signed for its latest
    /// attempt (docs/ci.md W27-R05).
    pub async fn check(&self, job: &str, idx: u32, signed: &kiln_ci::logs::Chain) -> kiln_ci::logs::Verdict {
        let log = self.read(job, idx).await;
        let pieces = self.pieces(job, idx).await;
        kiln_ci::logs::check(&log, pieces.last().map(Vec::as_slice).unwrap_or_default(), signed)
    }

    /// The last `max` bytes of a step's log on disk (all of it when it's shorter).
    pub async fn tail(&self, job: &str, idx: u32, max: u64) -> Vec<u8> {
        let p = self.step_path(job, idx);
        crate::rt::blocking(move || tail_of(&p, max).unwrap_or_default()).await.unwrap_or_default()
    }

    /// A line after what a step's log holds, where one attempt ends and the next begins: a job
    /// retried in place keeps its logs (docs/ci.md W05-R28). Written to the file only; a step
    /// with no log yet gets none.
    pub async fn mark(&self, job: &str, idx: u32, line: &str) {
        let path = self.step_path(job, idx);
        let digests = self.digests_path(job, idx);
        let text = format!("\r\n\x1b[33m[kiln] {line}\x1b[0m\r\n");
        let boundary = kiln_ci::logs::boundary(line);
        let r = crate::rt::blocking(move || -> std::io::Result<()> {
            if std::fs::metadata(&path).map_or(0, |m| m.len()) == 0 {
                return Ok(());
            }
            std::fs::OpenOptions::new().append(true).open(&path)?.write_all(text.as_bytes())?;
            // The next attempt's pieces start after it: each attempt is a leg of its own.
            match std::fs::OpenOptions::new().append(true).open(&digests) {
                Ok(mut f) => writeln!(f, "{boundary}"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            }
        })
        .await;
        if let Ok(Err(e)) | Err(e) = r.map_err(|e| std::io::Error::other(format!("{e:#}"))) {
            tracing::warn!(error = %e, job, "a retry's mark in the log failed");
        }
    }

    /// Size of the backlog already on disk (for the UI's "download" affordance).
    pub async fn size(&self, job: &str, idx: u32) -> u64 {
        let p = self.step_path(job, idx);
        crate::rt::blocking(move || std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)).await.unwrap_or(0)
    }

    pub async fn remove(&self, jobs: Vec<String>) {
        let dirs: Vec<PathBuf> = jobs.iter().map(|j| self.job_dir(j)).collect();
        let r = crate::rt::blocking(move || {
            for d in dirs {
                if let Err(e) = std::fs::remove_dir_all(&d)
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    tracing::warn!(error = %e, dir = %d.display(), "log prune failed");
                }
            }
        })
        .await;
        if let Err(e) = r {
            tracing::warn!(error = %e, "log prune task failed");
        }
    }
}

/// Notes the leg's chunk `p` in its step's `<step>.digests` (opened once, kept open while the step
/// writes). Never fails the caller: a disk error is logged.
fn note_piece(live: &mut Live, idx: u32, path: &Path, p: &kiln_ci::logs::Piece) {
    let file = match live.digests.entry(idx) {
        std::collections::hash_map::Entry::Occupied(o) => Some(o.into_mut()),
        std::collections::hash_map::Entry::Vacant(v) => match path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::OpenOptions::new().create(true).append(true).open(path)) {
            Ok(f) => Some(v.insert(f)),
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "cannot open log digests");
                None
            }
        },
    };
    if let Some(f) = file
        && let Err(e) = writeln!(f, "{}", kiln_ci::logs::line(p))
    {
        tracing::warn!(error = %e, "log digests write failed");
    }
}

/// `<step>.times` read: a line each, its offset and its ms; a line torn by a crash is left out.
fn times_of(s: &str) -> Vec<(u64, i64)> {
    s.lines()
        .filter_map(|l| {
            let (at, ms) = l.split_once(' ')?;
            Some((at.parse().ok()?, ms.parse().ok()?))
        })
        .collect()
}

/// [`Logs::tail`]'s read, from where the last `max` bytes start.
fn tail_of(p: &std::path::Path, max: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(p)?;
    let len = f.metadata()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(max)))?;
    let mut out = Vec::new();
    f.take(max).read_to_end(&mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_tail_and_cap() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
        let dir = tempfile::tempdir()?;
        let logs = Logs::new(dir.path().to_path_buf(), 16);
        let mut rx = logs.subscribe("job1");
        logs.append("job1", 0, b"hello ");
        logs.append("job1", 0, b"world");
        logs.append("job1", 0, b"this exceeds the cap");
        logs.append("job1", 0, b"dropped");
        logs.finish("job1");
        let on_disk = logs.read("job1", 0).await;
        assert!(on_disk.starts_with(b"hello world"));
        assert!(String::from_utf8_lossy(&on_disk).contains("limit reached"));
        // Its end, for what a failed step lacked (toolfix): the last bytes, or all of a short one.
        assert_eq!(Some(logs.tail("job1", 0, 5).await.as_slice()), on_disk.get(on_disk.len().saturating_sub(5)..));
        assert_eq!(logs.tail("job1", 0, 1 << 20).await, on_disk);
        assert!(logs.tail("job2", 0, 5).await.is_empty());
        let mut n = 0;
        while let Ok(ev) = rx.recv().await {
            n += 1;
            if matches!(ev, LogEvent::End) {
                break;
            }
        }
        assert_eq!(n, 4);
        Ok(())
    })
    }

    /// W27-R05: each chunk a leg sends is kept by its digest, past the log's cap too, so the log
    /// kiln keeps checks against the leg's chain: capped, it verifies what it kept by its bytes and
    /// the rest by digest; a byte changed on disk since is found; a retry in place starts another
    /// attempt's pieces, checked against that attempt's chain.
    #[test]
    fn w27_r05_the_server_keeps_each_chunk_by_digest_past_its_cap() -> anyhow::Result<()> {
        use kiln_ci::logs::{Chain, Verdict};
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let logs = Logs::new(dir.path().to_path_buf(), 24);
            let sent: [&[u8]; 4] = [b"$ cargo test\n", b"ok 1\n", b"this crosses the cap\n", b"dropped\n"];
            for c in sent {
                logs.append("job1", 0, c);
            }
            logs.step("job1", 0, "success");
            let signed = Chain::of(sent);
            let [first, second, ..] = sent;
            let kept = (first.len() + second.len()) as u64;
            assert_eq!(logs.check("job1", 0, &signed).await, Verdict::Verified { kept, dropped: 2 });
            assert_eq!(logs.pieces("job1", 0).await.first().map(Vec::len), Some(4));
            // A chain of other chunks, or of more, isn't what kiln took.
            assert_eq!(logs.check("job1", 0, &Chain::of([first, b"ok 2\n".as_slice()])).await, Verdict::Differs);
            let mut more = signed;
            more.push(b"never arrived\n");
            assert_eq!(logs.check("job1", 0, &more).await, Verdict::Incomplete { have: 4, want: 5 });
            // A byte changed on disk afterwards.
            let path = dir.path().join("b1").join("job1").join("0.log");
            let mut on_disk = std::fs::read(&path)?;
            if let Some(b) = on_disk.get_mut(first.len() + 1) {
                *b ^= 0x20;
            }
            std::fs::write(&path, &on_disk)?;
            assert_eq!(logs.check("job1", 0, &signed).await, Verdict::Altered { piece: 1, at: first.len() as u64 });
            // A retry in place: its pieces are the next attempt's, checked against its own chain.
            let logs = Logs::new(dir.path().to_path_buf(), 1 << 20);
            logs.append("job2", 0, b"attempt one\n");
            logs.step("job2", 0, "failure");
            logs.mark("job2", 0, "attempt 2").await;
            logs.append("job2", 0, b"attempt two\n");
            logs.finish("job2");
            let attempts = logs.pieces("job2", 0).await;
            assert_eq!(attempts.iter().map(Vec::len).collect::<Vec<_>>(), vec![1, 1]);
            let second_attempt = Chain::of([b"attempt two\n".as_slice()]);
            assert_eq!(logs.check("job2", 0, &second_attempt).await, Verdict::Verified { kept: 12, dropped: 0 });
            assert_eq!(logs.check("job2", 0, &Chain::of([b"attempt one\n".as_slice()])).await, Verdict::Differs);
            // A log from before kiln kept digests has no pieces: a signed chain finds none of it.
            assert_eq!(logs.check("job3", 0, &signed).await, Verdict::Incomplete { have: 0, want: 4 });
            Ok(())
        })
    }

    /// W27-R05: what kiln copies into a step's log (a reused step's, from its source leg) is kept
    /// and read as the rest is, and is no chunk of the leg's: the step's log checks against what the
    /// leg itself sent, and the steps kept by digest are those the leg sent a log of.
    #[test]
    fn w27_r05_a_copy_kiln_makes_is_kept_and_no_chunk_of_the_legs() -> anyhow::Result<()> {
        use kiln_ci::logs::{Chain, Verdict};
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let logs = Logs::new(dir.path().to_path_buf(), 1 << 20);
            let note: &[u8] = b"memo: reused from run #1\n";
            logs.append_copy("job1", 0, b"built 1 as 7f3a\n");
            logs.append("job1", 0, note);
            logs.append_copy("job1", 1, b"tested\n");
            logs.finish("job1");
            assert_eq!(logs.read("job1", 0).await, [b"built 1 as 7f3a\n".as_slice(), note].concat());
            assert_eq!(logs.check("job1", 0, &Chain::of([note])).await, Verdict::Verified { kept: note.len() as u64, dropped: 0 });
            assert_eq!(logs.kept_steps("job1").await, [0]);
            Ok(())
        })
    }

    /// Plan 01a1856fe81375b1bd1d1d86f270a45b, step 9: each piece of a step's log is noted with
    /// where it starts and when it came, as written, in order; a retry's mark between pieces
    /// moves the next one's offset, and a torn line in the file is left out.
    #[test]
    fn each_piece_of_a_log_is_noted_with_where_it_starts_and_when() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let logs = Logs::new(dir.path().to_path_buf(), 1 << 20);
            let before = crate::util::now_ms();
            logs.append("job1", 0, b"$ cargo clippy\n");
            logs.append("job1", 0, b"    Checking kiln\n");
            logs.mark("job1", 0, "attempt 2").await;
            logs.append("job1", 0, b"done\n");
            logs.append("job1", 1, b"step two\n");
            logs.finish("job1");
            let log = logs.read("job1", 0).await;
            let times = logs.times("job1", 0).await;
            let offsets: Vec<u64> = times.iter().map(|(at, _)| *at).collect();
            let done = log.windows(5).position(|w| w == b"done\n").and_then(|i| u64::try_from(i).ok());
            assert_eq!(offsets, vec![0, 15, done.unwrap_or(u64::MAX)]);
            assert!(times.iter().all(|(_, ms)| *ms >= before) && times.windows(2).all(|w| w.first().map(|a| a.1) <= w.get(1).map(|b| b.1)), "{times:?}");
            assert_eq!(logs.times("job1", 1).await.iter().map(|(at, _)| *at).collect::<Vec<_>>(), vec![0]);
            assert!(logs.times("job2", 0).await.is_empty());
            assert_eq!(times_of("0 1700000000000\n12 17000000\nnot a line\n40"), vec![(0, 1_700_000_000_000), (12, 17_000_000)]);
            Ok(())
        })
    }
}
