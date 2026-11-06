//! The harness's own record of a conversation, kept in the store (`docs/conversations.md`, "What is
//! kept"): Claude Code's transcript, Codex's rollouts. Followed on the session's machine as the
//! files grow, line by line, so the store has them, the feed takes them to the server, and a
//! session whose machine is gone can go on elsewhere from the server's copy.
//!
//! Lines are numbered among the file's non-empty ones, as a move carries them
//! (`harness::moving::record`): a copy of the file on another machine numbers them alike. Where
//! following a file goes on after a restart is where the store's copy of it ends, checked against
//! the file: one that was replaced since (shorter, or not ending with the line kept) starts over.

use std::collections::HashMap;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use super::rows::{RecordRow, Row};
use super::store::Store;
use crate::harness::kinds::{Kind, Protocol};

/// The latest any session here flushed its harness's updates, unix ms ([`active`]): its harness wrote, so its record's
/// files may have grown. The recorder looks at a session's files as it learns that (consts-audit B3: it looked at every
/// file every second, `TICK`).
static ACTIVE: AtomicI64 = AtomicI64::new(0);

/// A session here flushed its harness's updates at `at_ms` (unix ms, `crate::harness::session`): the recorder looks at
/// its files once the store has taken them in ([`Recorder`]).
pub fn active(at_ms: i64) {
    ACTIVE.fetch_max(at_ms, Ordering::Relaxed);
}
/// Read from one file at once, at the most (a long file comes in over several looks).
const READ_MAX: u64 = 8 << 20;
/// A line longer than a read is read whole, up to this.
const LINE_MAX: u64 = 256 << 20;
/// How often a session's files are looked for again (a Codex thread's next rollout, a transcript
/// not there yet).
const FIND_EVERY: Duration = Duration::from_secs(15);

/// What a session's harness keeps as its record, as the session says.
#[derive(Clone)]
pub struct Source {
    /// kiln's id of the session.
    pub session: String,
    /// The harness it runs in now.
    pub kind: &'static Kind,
    /// The environment it runs with (where the harness keeps its files).
    pub env: Vec<(String, String)>,
    /// The harness's own session (Claude Code's session id, Codex's thread id).
    pub harness_session: String,
    /// When the session last flushed its harness's updates (unix ms): its files are looked at once after it.
    pub active_ms: i64,
}

/// The files of a session's record: `(source, path)`.
fn files_of(src: &Source) -> Vec<(String, PathBuf)> {
    match src.kind.protocol {
        Protocol::ClaudeStreamJson => {
            let config = crate::harness::transcript::config_dir(&src.env);
            crate::harness::transcript::find(&config, &src.harness_session).map(|p| vec![(src.harness_session.clone(), p)]).unwrap_or_default()
        }
        Protocol::CodexAppServer => {
            let home = crate::harness::handoff::codex_home(&src.env);
            crate::harness::handoff::codex_files(&home, &src.harness_session)
                .into_iter()
                .filter_map(|p| p.file_name().map(|n| (n.to_string_lossy().into_owned(), p.clone())))
                .collect()
        }
        Protocol::Acp(_) | Protocol::AgyStreamJson | Protocol::Terminal => Vec::new(),
    }
}

/// One file followed.
#[derive(Debug)]
struct Followed {
    path: PathBuf,
    /// The next line's number.
    n: u64,
    /// Read through here (just past a newline).
    end: u64,
    /// The line the store's copy ends with, to be checked against the file before going on.
    check: Option<String>,
    /// When it was last looked at (none yet), and that in unix ms (against its session's last flush, `Source::active_ms`).
    looked: Option<Instant>,
    looked_ms: i64,
    /// When a look last found it grown (or it was first followed).
    grew: Instant,
    /// The time between its lines, as its looks find them ([`Followed::looked_at`]).
    gap: Gap,
    /// When it's looked at next, unless its session flushes first.
    next: Instant,
}

impl Followed {
    fn new(path: &Path, n: u64, end: u64, check: Option<String>) -> Self {
        let now = Instant::now();
        Self { path: path.to_owned(), n, end, check, looked: None, looked_ms: 0, grew: now, gap: Gap::default(), next: now }
    }

    /// Looked at `now` (`now_ms`, unix ms), `lines` new lines found, the look taking `took`: when it's looked at next
    /// (consts-audit B3: every file every second, `TICK`). While it grows, at its own pace: the time between its lines, as
    /// each look that finds some teaches it (the time since the last look over the lines it found). Once quiet, as long
    /// again as it's been quiet, so a quiet file is looked at less and less often and a line written after a quiet spell is
    /// found within as long as the spell. Never sooner than a look takes.
    fn looked_at(&mut self, now: Instant, now_ms: i64, lines: u64, took: Duration) {
        if lines > 0 {
            if let Some(last) = self.looked {
                self.gap.learn(nanos(now.saturating_duration_since(last)) / lines);
            }
            self.grew = now;
        }
        (self.looked, self.looked_ms) = (Some(now), now_ms);
        let wait = Duration::from_nanos(self.gap.mean.unwrap_or(0)).max(now.saturating_duration_since(self.grew)).max(took);
        self.next = now + wait;
    }
}

/// Whether a file is looked at `now`: one not followed yet, one whose session flushed its harness's updates since its last
/// look (`active_ms`, unix ms), one due by its own pace ([`Followed::looked_at`]).
fn due(f: Option<&Followed>, active_ms: i64, now: Instant) -> bool {
    f.is_none_or(|f| active_ms >= f.looked_ms || now >= f.next)
}

/// The time between a file's lines, nanoseconds, learned as RFC 6298 learns a round trip (the mean an eighth of the way to
/// each sample, the deviation a quarter; the first sets the mean, half of it the deviation), but a fall taken at once: a
/// file that writes faster is looked at as often from its next look.
#[derive(Clone, Copy, Debug, Default)]
struct Gap {
    mean: Option<u64>,
    dev: u64,
}

impl Gap {
    fn learn(&mut self, x: u64) {
        match self.mean {
            Some(m) if x >= m => {
                self.dev = self.dev - self.dev / 4 + x.abs_diff(m) / 4;
                self.mean = Some(m - m / 8 + x / 8);
            }
            _ => *self = Gap { mean: Some(x), dev: x / 2 },
        }
    }
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// A session's files, as last found.
struct Found {
    harness_session: String,
    files: Vec<(String, PathBuf)>,
    at: Instant,
}

/// Follows the record of every session `sources` names, into `store`. Its own thread, which waits for the store's next
/// commit (a session's flush kept: [`active`]) or the next file due by its own pace ([`Followed::looked_at`]), and looks
/// when a session flushed since it last looked, a file is due, or a session's files couldn't be started on: none every
/// second (consts-audit B3: `TICK`). What it reads goes to the store one put at a time, so a look takes in what was
/// written while the last was kept.
pub struct Recorder;

impl Recorder {
    pub fn start(store: Store, sources: impl Fn() -> Vec<Source> + Send + 'static) -> anyhow::Result<Self> {
        std::thread::Builder::new().name("kiln-convo-record".into()).spawn(move || {
            let mut r = Following::new(store);
            loop {
                // Told of the commits from here on: one while it looks ends the wait after at once.
                let store = r.store.clone();
                let commits = store.commits();
                let (active, now) = (ACTIVE.load(Ordering::Relaxed), Instant::now());
                if active != r.active || r.unstarted || r.next.is_none_or(|n| now >= n) {
                    r.active = active;
                    r.next = r.look(&sources(), now);
                }
                match r.next {
                    Some(at) => {
                        let _ = commits.wait_deadline(at);
                    }
                    None => commits.wait(),
                }
            }
        })?;
        Ok(Self)
    }
}

/// What the recorder knows.
struct Following {
    store: Store,
    /// Per session, per source.
    files: HashMap<(String, String), Followed>,
    found: HashMap<String, Found>,
    /// The sessions' last flush as it last looked ([`ACTIVE`]); when a file's due next, if any; whether a file couldn't be
    /// started on (its store couldn't say where its copy ends), asked again at the next commit.
    active: i64,
    next: Option<Instant>,
    unstarted: bool,
}

impl Following {
    fn new(store: Store) -> Self {
        Self { store, files: HashMap::new(), found: HashMap::new(), active: 0, next: None, unstarted: false }
    }

    /// A look at what's due of every live session's files at `now`: a new file, a file whose session flushed since it was
    /// last looked at, a file due by its own pace; and the session's files found again where that's due. When the next is
    /// due, if any.
    fn look(&mut self, live: &[Source], now: Instant) -> Option<Instant> {
        let mut next: Option<Instant> = None;
        let mut soonest = |at: Instant| next = Some(next.map_or(at, |n| n.min(at)));
        self.unstarted = false;
        for src in live {
            // Found again when the harness's session changed, and now and then while there may be
            // more to find: a transcript not there yet, a Codex thread's next rollout.
            let stale = self.found.get(&src.session).is_none_or(|f| {
                f.harness_session != src.harness_session
                    || (f.at.elapsed() >= FIND_EVERY && (f.files.is_empty() || src.kind.protocol == Protocol::CodexAppServer || f.files.iter().any(|(_, p)| !p.is_file())))
            });
            if stale {
                let files = files_of(src);
                self.found.insert(src.session.clone(), Found { harness_session: src.harness_session.clone(), files, at: Instant::now() });
            }
            let Some(found) = self.found.get(&src.session) else { continue };
            if found.files.is_empty() || src.kind.protocol == Protocol::CodexAppServer || found.files.iter().any(|(_, p)| !p.is_file()) {
                soonest(found.at + FIND_EVERY);
            }
            for (source, path) in found.files.clone() {
                let key = (src.session.clone(), source.clone());
                if due(self.files.get(&key), src.active_ms, now) {
                    self.follow(&src.session, &source, &path);
                }
                match self.files.get(&key) {
                    Some(f) => soonest(f.next),
                    None => self.unstarted = true,
                }
            }
        }
        // A session gone from here (ended, moved on): one last look at what it wrote, then let go.
        let gone: Vec<String> = self.found.keys().filter(|s| !live.iter().any(|l| &l.session == *s)).cloned().collect();
        for s in gone {
            if let Some(f) = self.found.remove(&s) {
                for (source, path) in f.files {
                    self.follow(&s, &source, &path);
                }
            }
            self.files.retain(|(sess, _), _| sess != &s);
        }
        next
    }

    /// Where following `source` of `session` starts: where the store's copy ends. None while the
    /// store can't say (busy, or stopped): asked again next look. Taken for "nothing kept", it
    /// read the whole file again, and the store and every feed of it took it all again.
    fn start(&self, session: &str, source: &str, path: &Path) -> Option<Followed> {
        let marks = match self.store.marks(session) {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(%session, %source, error = %format!("{e:#}"), "conversation store: where its copy of the harness's record ends couldn't be read; asked again");
                return None;
            }
        };
        Some(match marks.into_iter().find(|m| m.source == source) {
            Some(m) => Followed::new(path, m.n + 1, m.end, Some(m.line)),
            None => Followed::new(path, 0, 0, None),
        })
    }

    /// What `source` of `session` has that the store hasn't, into the store.
    fn follow(&mut self, session: &str, source: &str, path: &Path) {
        let key = (session.to_owned(), source.to_owned());
        if !self.files.contains_key(&key) {
            let Some(f) = self.start(session, source, path) else { return };
            self.files.insert(key.clone(), f);
        }
        let Some(f) = self.files.get_mut(&key) else { return };
        if f.path != path {
            // The file moved (a session that went on in another folder): check it again.
            f.path = path.to_owned();
        }
        let mut rows = Vec::new();
        let (n0, end0) = (f.n, f.end);
        let began = Instant::now();
        match read_new(f) {
            Ok(Read { replaced, lines }) => {
                if replaced {
                    rows.push(Row::RecordReset { session: session.to_owned(), source: source.to_owned(), at: crate::util::now_ms() });
                }
                for (n, end, line) in lines {
                    rows.push(Row::Record(Box::new(RecordRow { session: session.to_owned(), source: source.to_owned(), n, end, line, kept: None })));
                }
            }
            Err(e) => {
                tracing::debug!(%session, %source, error = %e, "conversation store: the harness's record couldn't be read");
                f.looked_at(Instant::now(), crate::util::now_ms(), 0, began.elapsed());
                return;
            }
        }
        let lines = rows.iter().filter(|r| matches!(r, Row::Record(_))).count();
        // Not handed over (the store is stuck): read again next time.
        if !self.store.put_wait(rows) {
            f.n = n0;
            f.end = end0;
        }
        f.looked_at(Instant::now(), crate::util::now_ms(), u64::try_from(lines).unwrap_or(u64::MAX), began.elapsed());
    }
}

/// What a look at a file found.
struct Read {
    /// It was replaced since the store's copy: that starts over.
    replaced: bool,
    /// `(n, end, line)`.
    lines: Vec<(u64, u64, String)>,
}

/// The file's complete lines past `f.end`, moving `f` past them.
fn read_new(f: &mut Followed) -> std::io::Result<Read> {
    let mut file = std::fs::File::open(&f.path)?;
    let len = file.metadata()?.len();
    let mut replaced = false;
    if let Some(line) = f.check.take() {
        if !ends_with_line(&mut file, f.end, &line)? {
            replaced = true;
        }
    } else if len < f.end {
        replaced = true;
    }
    if replaced {
        f.n = 0;
        f.end = 0;
    }
    let mut lines = Vec::new();
    if len <= f.end {
        return Ok(Read { replaced, lines });
    }
    let mut want = (len - f.end).min(READ_MAX);
    let buf = loop {
        let mut buf = vec![0u8; usize::try_from(want).unwrap_or(usize::MAX)];
        file.seek(SeekFrom::Start(f.end))?;
        file.read_exact(&mut buf)?;
        // A line longer than what was read: read more of it, while there is more.
        if buf.contains(&b'\n') || f.end + want >= len || want >= LINE_MAX {
            break buf;
        }
        want = (want * 4).min(len - f.end).min(LINE_MAX);
    };
    let mut at = 0usize;
    while let Some(nl) = buf.get(at..).and_then(|rest| rest.iter().position(|b| *b == b'\n')) {
        let raw = buf.get(at..at + nl).unwrap_or_default();
        let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
        at += nl + 1;
        let end = f.end + at as u64;
        if raw.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        lines.push((f.n, end, String::from_utf8_lossy(raw).into_owned()));
        f.n += 1;
    }
    f.end += at as u64;
    Ok(Read { replaced, lines })
}

/// The file's line that ends just before `end` (its newline, and a `\r` before that, aside) is
/// `line`.
fn ends_with_line(file: &mut std::fs::File, end: u64, line: &str) -> std::io::Result<bool> {
    if end == 0 || file.metadata()?.len() < end {
        return Ok(false);
    }
    // The line, its `\r\n`, and the newline before it.
    let start = end.saturating_sub(line.len() as u64 + 3);
    let mut buf = vec![0u8; usize::try_from(end - start).unwrap_or(0)];
    file.seek(SeekFrom::Start(start))?;
    file.read_exact(&mut buf)?;
    let Some(b) = buf.strip_suffix(b"\n") else { return Ok(false) };
    let b = b.strip_suffix(b"\r").unwrap_or(b);
    let Some(before) = b.strip_suffix(line.as_bytes()) else { return Ok(false) };
    Ok(match before.last() {
        None => start == 0,
        Some(c) => *c == b'\n',
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn rows_of(store: &Store, session: &str, source: &str) -> anyhow::Result<Vec<(u64, String)>> {
        store.sync()?;
        store.record(session, source, 0, 10_000)
    }

    #[test]
    fn a_growing_file_is_kept_line_by_line_and_a_half_written_line_waits() -> anyhow::Result<()> {
        let (dir, store) = super::super::store::scratch()?;
        let path = dir.path().join("t.jsonl");
        std::fs::write(&path, "{\"a\":1}\n\n{\"b\":2}\r\n{\"c\":")?;
        let mut r = Following::new(store.clone());
        r.follow("s1", "t", &path);
        // The blank line has no number; the line not ended yet isn't read.
        assert_eq!(rows_of(&store, "s1", "t")?, vec![(0, "{\"a\":1}".to_owned()), (1, "{\"b\":2}".to_owned())]);
        std::fs::OpenOptions::new().append(true).open(&path)?.write_all(b"3}\n{\"d\":4}\n")?;
        r.follow("s1", "t", &path);
        assert_eq!(rows_of(&store, "s1", "t")?.into_iter().map(|(n, _)| n).collect::<Vec<_>>(), vec![0, 1, 2, 3]);
        assert_eq!(rows_of(&store, "s1", "t")?.get(2).map(|(_, l)| l.clone()), Some("{\"c\":3}".to_owned()));
        Ok(())
    }

    #[test]
    fn a_store_that_cannot_say_where_its_copy_ends_is_asked_again_and_nothing_is_read_twice() -> anyhow::Result<()> {
        let (dir, store) = super::super::store::scratch()?;
        let path = dir.path().join("t.jsonl");
        std::fs::write(&path, "one\ntwo\n")?;
        Following::new(store.clone()).follow("s1", "t", &path);
        store.sync()?;
        let top = store.top()?;
        // Restarted while its store can't answer (busy, or stopped): the file isn't taken to be
        // new to it, read from its top and sent to the store again.
        let mut stuck = Following::new(super::super::store::stopped());
        stuck.follow("s1", "t", &path);
        assert!(stuck.files.is_empty());
        // Asked again once it answers: it goes on where the store's copy ends.
        std::fs::OpenOptions::new().append(true).open(&path)?.write_all(b"three\n")?;
        let mut again = Following::new(store.clone());
        again.follow("s1", "t", &path);
        assert_eq!(rows_of(&store, "s1", "t")?, vec![(0, "one".to_owned()), (1, "two".to_owned()), (2, "three".to_owned())]);
        assert_eq!(store.top()?, top + 1);
        Ok(())
    }

    #[test]
    fn after_a_restart_it_goes_on_from_the_store_and_a_replaced_file_starts_over() -> anyhow::Result<()> {
        let (dir, store) = super::super::store::scratch()?;
        let path = dir.path().join("t.jsonl");
        std::fs::write(&path, "one\ntwo\n")?;
        Following::new(store.clone()).follow("s1", "t", &path);
        store.sync()?;
        // Restarted: a new recorder, the file grew meanwhile.
        std::fs::OpenOptions::new().append(true).open(&path)?.write_all(b"three\n")?;
        let mut again = Following::new(store.clone());
        again.follow("s1", "t", &path);
        assert_eq!(rows_of(&store, "s1", "t")?, vec![(0, "one".to_owned()), (1, "two".to_owned()), (2, "three".to_owned())]);
        // Replaced by another file of the same length or longer: not the same lines, so it starts over.
        std::fs::write(&path, "uno\ndos\ntres\ncuatro\n")?;
        let mut third = Following::new(store.clone());
        third.follow("s1", "t", &path);
        assert_eq!(rows_of(&store, "s1", "t")?.into_iter().map(|(_, l)| l).collect::<Vec<_>>(), vec!["uno", "dos", "tres", "cuatro"]);
        Ok(())
    }

    /// Consts-audit B3: a file's looks follow its own pace, where every file was looked at every second (`TICK`). Do: looks
    /// 100 ms apart finding five lines each; then looks finding none; then a look finding a burst; then a look that took
    /// longer than its pace. Expect: while it grows, the next look at its pace (20 ms: nothing added); quiet, each after as
    /// long again as it's been quiet (20, 40, 80, 160 ms); a burst after the quiet spell, its pace at once (5 ms); never
    /// sooner than a look takes.
    #[test]
    fn b3_a_files_looks_follow_its_own_pace_and_space_out_as_it_goes_quiet() {
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        let mut f = Followed::new(Path::new("t.jsonl"), 0, 0, None);
        f.looked_at(t0, 1, 5, ms(1));
        assert_eq!(f.next, t0 + ms(1), "a first look: nothing to learn a pace from, the next as soon as a look takes");
        let mut t = t0;
        for _ in 0..4 {
            t += ms(100);
            f.looked_at(t, 1, 5, Duration::from_micros(10));
            assert_eq!(f.next, t + ms(20), "growing: at its pace");
        }
        let mut waits = Vec::new();
        for _ in 0..4 {
            let at = f.next;
            f.looked_at(at, 1, 0, Duration::from_micros(10));
            waits.push(f.next.saturating_duration_since(at));
        }
        assert_eq!(waits, [ms(20), ms(40), ms(80), ms(160)], "quiet: as long again as it's been quiet");
        let at = f.next;
        f.looked_at(at, 1, 32, Duration::from_micros(10));
        assert_eq!(f.next, at + ms(5), "a burst after the quiet spell: 160 ms over 32 lines, at once");
        f.looked_at(at + ms(5), 1, 1, ms(30));
        assert_eq!(f.next, at + ms(35), "never sooner than a look takes");
    }

    /// Consts-audit B3: a file is looked at once its session flushes its harness's updates after its last look, or once
    /// it's due by its own pace; a new file at once; else not. Do: a file last looked at (unix ms 1,000), its next far
    /// off. Expect: due with its session's last flush at or after its look, or a new file, or past its next; not with
    /// the flush before its look and its next ahead.
    #[test]
    fn b3_a_file_is_looked_at_once_its_session_flushes_or_its_pace_says() {
        let now = Instant::now();
        let mut f = Followed::new(Path::new("t.jsonl"), 0, 0, None);
        f.looked_at(now, 1_000, 1, Duration::from_secs(60));
        assert!(due(None, 0, now), "a new file");
        assert!(!due(Some(&f), 999, now), "flushed before its last look, its next ahead");
        assert!(due(Some(&f), 1_000, now) && due(Some(&f), 5_000, now), "flushed since its last look");
        assert!(due(Some(&f), 999, now + Duration::from_secs(61)), "past its next");
    }

    #[test]
    fn a_line_longer_than_a_read_comes_whole() -> anyhow::Result<()> {
        let (dir, store) = super::super::store::scratch()?;
        let path = dir.path().join("t.jsonl");
        let long = "z".repeat(usize::try_from(READ_MAX).unwrap_or(0) + 10);
        std::fs::write(&path, format!("{long}\nshort\n"))?;
        let mut r = Following::new(store.clone());
        r.follow("s1", "t", &path);
        r.follow("s1", "t", &path);
        let got = rows_of(&store, "s1", "t")?;
        assert_eq!(got.iter().map(|(_, l)| l.len()).collect::<Vec<_>>(), vec![long.len(), 5]);
        Ok(())
    }
}
