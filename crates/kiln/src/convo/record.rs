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
use std::time::{Duration, Instant};

use super::rows::{RecordRow, Row};
use super::store::Store;
use crate::harness::kinds::{Kind, Protocol};

/// How often the files are looked at.
pub const TICK: Duration = Duration::from_secs(1);
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
}

/// A session's files, as last found.
struct Found {
    harness_session: String,
    files: Vec<(String, PathBuf)>,
    at: Instant,
}

/// Follows the record of every session `sources` names, into `store`. Its own thread.
pub struct Recorder;

impl Recorder {
    pub fn start(store: Store, sources: impl Fn() -> Vec<Source> + Send + 'static) -> anyhow::Result<Self> {
        std::thread::Builder::new().name("kiln-convo-record".into()).spawn(move || {
            let mut r = Following { store, files: HashMap::new(), found: HashMap::new() };
            loop {
                r.look(&sources());
                std::thread::sleep(TICK);
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
}

impl Following {
    /// One look at every live session's files.
    fn look(&mut self, live: &[Source]) {
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
            let files = self.found.get(&src.session).map(|f| f.files.clone()).unwrap_or_default();
            for (source, path) in files {
                self.follow(&src.session, &source, &path);
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
            Some(m) => Followed { path: path.to_owned(), n: m.n + 1, end: m.end, check: Some(m.line) },
            None => Followed { path: path.to_owned(), n: 0, end: 0, check: None },
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
                return;
            }
        }
        // Not handed over (the store is stuck): read again next time.
        if !self.store.put_wait(rows) {
            f.n = n0;
            f.end = end0;
        }
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
        let mut r = Following { store: store.clone(), files: HashMap::new(), found: HashMap::new() };
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
        Following { store: store.clone(), files: HashMap::new(), found: HashMap::new() }.follow("s1", "t", &path);
        store.sync()?;
        let top = store.top()?;
        // Restarted while its store can't answer (busy, or stopped): the file isn't taken to be
        // new to it, read from its top and sent to the store again.
        let mut stuck = Following { store: super::super::store::stopped(), files: HashMap::new(), found: HashMap::new() };
        stuck.follow("s1", "t", &path);
        assert!(stuck.files.is_empty());
        // Asked again once it answers: it goes on where the store's copy ends.
        std::fs::OpenOptions::new().append(true).open(&path)?.write_all(b"three\n")?;
        let mut again = Following { store: store.clone(), files: HashMap::new(), found: HashMap::new() };
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
        Following { store: store.clone(), files: HashMap::new(), found: HashMap::new() }.follow("s1", "t", &path);
        store.sync()?;
        // Restarted: a new recorder, the file grew meanwhile.
        std::fs::OpenOptions::new().append(true).open(&path)?.write_all(b"three\n")?;
        let mut again = Following { store: store.clone(), files: HashMap::new(), found: HashMap::new() };
        again.follow("s1", "t", &path);
        assert_eq!(rows_of(&store, "s1", "t")?, vec![(0, "one".to_owned()), (1, "two".to_owned()), (2, "three".to_owned())]);
        // Replaced by another file of the same length or longer: not the same lines, so it starts over.
        std::fs::write(&path, "uno\ndos\ntres\ncuatro\n")?;
        let mut third = Following { store: store.clone(), files: HashMap::new(), found: HashMap::new() };
        third.follow("s1", "t", &path);
        assert_eq!(rows_of(&store, "s1", "t")?.into_iter().map(|(_, l)| l).collect::<Vec<_>>(), vec!["uno", "dos", "tres", "cuatro"]);
        Ok(())
    }

    #[test]
    fn a_line_longer_than_a_read_comes_whole() -> anyhow::Result<()> {
        let (dir, store) = super::super::store::scratch()?;
        let path = dir.path().join("t.jsonl");
        let long = "z".repeat(usize::try_from(READ_MAX).unwrap_or(0) + 10);
        std::fs::write(&path, format!("{long}\nshort\n"))?;
        let mut r = Following { store: store.clone(), files: HashMap::new(), found: HashMap::new() };
        r.follow("s1", "t", &path);
        r.follow("s1", "t", &path);
        let got = rows_of(&store, "s1", "t")?;
        assert_eq!(got.iter().map(|(_, l)| l.len()).collect::<Vec<_>>(), vec![long.len(), 5]);
        Ok(())
    }
}
