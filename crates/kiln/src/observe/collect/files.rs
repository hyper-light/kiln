//! Files tailed (`docs/observe.md` §8.1), as Datadog's Agent and Vector tail them: the globs
//! looked at every [`SCAN`], each file read as it grows, from where its last written record ends.
//! A file is known by its identity (device and inode; off Unix, its creation time, which a rename
//! keeps) and by its leading bytes, so that:
//!
//! - renamed (logrotate's `create`), it is read to its end and closed once it stops growing;
//! - truncated or rewritten (logrotate's `copytruncate`), it is read again from its start, and its
//!   copy, whose leading [`HEAD`] bytes are its own, from where it had been read to;
//! - deleted while read, it is read to its end;
//! - a new file at an old path is read from its start, as is every file that appears after its
//!   source started (one there at the start, with no place kept, from its end unless `start` says
//!   `beginning`).
//!
//! Lines end at a newline (`\r\n` too); a last line without one is taken once the file has not
//! grown for [`PARTIAL_WAIT`]; a line longer than [`MAX_LINE`] is cut into records that long.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::{Batch, Compiled, Encoding, Format, InFlight, Kind, Line, MAX_LINE, Offer, Origin, Start, State, Stop};

/// How often the globs are looked at anew (Datadog's Agent's `file_scan_period`).
const SCAN: Duration = Duration::from_secs(10);
/// How long a round sleeps once no file had more.
const POLL: Duration = Duration::from_millis(250);
/// Bytes a file gives a round at the most, so that one busy file does not starve the rest.
const ROUND_BYTES: usize = 1 << 20;
/// A read's buffer.
const READ: usize = 64 << 10;
/// Files open at once at the most (Datadog's Agent's `open_files_limit` on Linux): past it, the
/// most recently written are read.
const OPEN_MAX: usize = 500;
/// A file whose path names it no more (rotated, deleted) is read while it grows, and closed once
/// it has not for this long.
const GONE_WAIT: Duration = Duration::from_secs(30);
/// A last line without its newline is taken once its file has not grown for this long.
const PARTIAL_WAIT: Duration = Duration::from_secs(5);
/// A record of several lines (a `multi_line` rule) is taken once no line came for this long
/// (Datadog's Agent's aggregation timeout).
const MULTI_WAIT: Duration = Duration::from_secs(1);
/// How often a file read to its end is looked at for being truncated.
const LOOK: Duration = Duration::from_secs(1);
/// The leading bytes a file is known by beside its identity.
pub const HEAD: usize = 1024;
/// How often places are written down.
const SAVE_EVERY: Duration = Duration::from_secs(1);
/// A place of a file not seen for this long is forgotten; this many are kept at the most.
const FORGET_MS: i64 = 7 * 86_400_000;
const PLACES_MAX: usize = 10_000;
/// Paths a scan takes at the most.
const MATCH_MAX: usize = 100_000;
/// How long a truncated file's leading bytes and place are kept for its copy to be known by.
const FORMER_WAIT: Duration = Duration::from_secs(600);
/// The most a docker container's `config.v2.json` is read of.
const CONFIG_MAX: u64 = 4 << 20;

/// What a file is, whatever its path: its device and inode on Unix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Key(pub u64, pub u64);

#[cfg(unix)]
fn key_of(m: &std::fs::Metadata, _path: &Path) -> Key {
    use std::os::unix::fs::MetadataExt;
    Key(m.dev(), m.ino())
}

/// Off Unix stable Rust has no file index: its creation time, which a rename keeps (its leading
/// bytes tell a new file that took an old one's from it); failing that, its path.
#[cfg(not(unix))]
fn key_of(m: &std::fs::Metadata, path: &Path) -> Key {
    match m.created().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()) {
        Some(d) => Key(u64::try_from(d.as_nanos()).unwrap_or(u64::MAX), 1),
        None => Key(fnv(path.to_string_lossy().as_bytes()), 2),
    }
}

fn fnv(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &x in b {
        h = (h ^ u64::from(x)).wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Where a file's records are written to, kept across restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Place {
    key: Key,
    path: String,
    /// Where the last record written ends.
    offset: u64,
    /// The hash of its first `head_len` bytes ([`HEAD`] once it has them).
    head: u64,
    head_len: u32,
    /// Counted up each time the file is found rewritten: a mark of before moves its place no more.
    #[serde(default)]
    generation: u32,
    /// Unix ms it was last seen.
    seen: i64,
}

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    places: Vec<Place>,
}

/// A record's mark: its file, the file's generation, where the record ends.
type Mark = (Key, u32, u64);

/// A file open.
struct Tail {
    key: Key,
    path: PathBuf,
    file: File,
    generation: u32,
    /// The next byte to read, and the file's size when last looked at.
    read: u64,
    size: u64,
    /// Where the last line handed on ends.
    emitted: u64,
    /// Bytes read and not yet a line, and the offset of the first of them.
    buf: Vec<u8>,
    buf_at: u64,
    grew: Instant,
    looked: Instant,
    /// Its path names it no more (rotated, deleted): read to its end, then closed.
    gone: bool,
    origin: Origin,
    /// A long line's parts (CRI's `P`, Docker's lines without their newline), joined.
    part: Option<Line>,
    /// A record of several lines being made, with where its last line ends and when it came.
    multi: Option<(Line, u64, Instant)>,
    /// Whether a byte order mark at its start was looked for.
    bom_done: bool,
}

/// Where a round's records go, and what it counted.
struct Sink {
    batch: Batch<Mark>,
    excluded: u64,
    truncated: u64,
    now_ns: i64,
}

fn mark(b: &mut Batch<Mark>, m: Mark) {
    match b.marks.iter_mut().find(|x| x.0 == m.0 && x.1 == m.1) {
        Some(x) => x.2 = x.2.max(m.2),
        None => b.marks.push(m),
    }
}

/// A byte order mark of `e`.
fn bom(e: Encoding) -> &'static [u8] {
    match e {
        Encoding::Utf8 => &[0xEF, 0xBB, 0xBF],
        Encoding::Utf16Le => &[0xFF, 0xFE],
        Encoding::Utf16Be => &[0xFE, 0xFF],
    }
}

fn decode(b: &[u8], e: Encoding) -> String {
    match e {
        Encoding::Utf8 => String::from_utf8_lossy(b).into_owned(),
        Encoding::Utf16Le | Encoding::Utf16Be => {
            let units: Vec<u16> = b.as_chunks::<2>().0.iter().map(|&[x, y]| if e == Encoding::Utf16Le { u16::from_le_bytes([x, y]) } else { u16::from_be_bytes([x, y]) }).collect();
            String::from_utf16_lossy(&units)
        }
    }
}

/// The longest prefix of `b` up to `max` bytes that ends at a character's end.
fn boundary(b: &[u8], max: usize, e: Encoding) -> usize {
    match e {
        Encoding::Utf8 => {
            let mut c = max.min(b.len());
            while c > 0 && c < b.len() && b.get(c).is_some_and(|&x| x & 0xC0 == 0x80) {
                c -= 1;
            }
            if c == 0 { max.min(b.len()) } else { c }
        }
        Encoding::Utf16Le | Encoding::Utf16Be => max.min(b.len()) & !1,
    }
}

/// A CRI log line: its time, stream, whether it is a part of a longer line, its text.
fn cri(line: &str) -> Option<(Option<i64>, &'static str, bool, &str)> {
    let (ts, rest) = line.split_once(' ')?;
    let (stream, rest) = rest.split_once(' ')?;
    let stream = match stream {
        "stdout" => "stdout",
        "stderr" => "stderr",
        _ => return None,
    };
    let (tag, text) = rest.split_once(' ').unwrap_or((rest, ""));
    let partial = match tag.split(':').next()? {
        "P" => true,
        "F" => false,
        _ => return None,
    };
    Some((crate::observe::cloud::parse_time_ns(ts), stream, partial, text))
}

#[derive(Deserialize)]
struct DockerLine {
    log: String,
    #[serde(default)]
    stream: String,
    #[serde(default)]
    time: String,
}

/// A Docker `json-file` line: its time, stream, whether it is a part of a longer line (no newline
/// at its end), its text.
fn docker(line: &str) -> Option<(Option<i64>, &'static str, bool, String)> {
    let mut d: DockerLine = serde_json::from_str(line).ok()?;
    let stream = if d.stream == "stderr" { "stderr" } else { "stdout" };
    let partial = !d.log.ends_with('\n');
    if !partial {
        d.log.pop();
        if d.log.ends_with('\r') {
            d.log.pop();
        }
    }
    Some((crate::observe::cloud::parse_time_ns(&d.time), stream, partial, d.log))
}

impl Tail {
    /// Reads what it has, a share of `budget` at the most, handing on its lines: the bytes read.
    fn read(&mut self, c: &Compiled, sink: &mut Sink, scratch: &mut [u8], budget: &mut usize) -> std::io::Result<usize> {
        let mut got = 0;
        while *budget > 0 {
            let want = scratch.len().min(*budget);
            let n = match scratch.get_mut(..want).map(|s| self.file.read(s)) {
                Some(Ok(n)) => n,
                Some(Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Some(Err(e)) => return Err(e),
                None => 0,
            };
            if n == 0 {
                break;
            }
            self.buf.extend_from_slice(scratch.get(..n).unwrap_or_default());
            self.read += n as u64;
            self.size = self.size.max(self.read);
            self.grew = Instant::now();
            *budget = budget.saturating_sub(n);
            got += n;
            self.lines(c, sink, false);
        }
        Ok(got)
    }

    /// Whether it was truncated (its size under what was read), looked at once a [`LOOK`] at the
    /// most.
    fn truncated(&mut self) -> std::io::Result<bool> {
        if self.looked.elapsed() < LOOK {
            return Ok(false);
        }
        self.looked = Instant::now();
        let len = self.file.metadata()?.len();
        self.size = len;
        Ok(len < self.read)
    }

    /// Read again from its start: a generation on, what was pending dropped.
    fn rewound(&mut self) -> std::io::Result<()> {
        self.file.seek(SeekFrom::Start(0))?;
        self.generation = self.generation.wrapping_add(1);
        self.read = 0;
        self.emitted = 0;
        self.buf.clear();
        self.buf_at = 0;
        self.part = None;
        self.multi = None;
        self.bom_done = false;
        Ok(())
    }

    /// Its complete lines handed on, and a line past [`MAX_LINE`] cut; with `flush`, what is left
    /// too (a last line without its newline).
    fn lines(&mut self, c: &Compiled, sink: &mut Sink, flush: bool) {
        if !self.bom_done && self.buf_at == 0 {
            let mark = bom(c.encoding);
            if self.buf.len() < mark.len() && mark.starts_with(&self.buf) && !flush {
                return;
            }
            if self.buf.starts_with(mark) {
                self.buf.drain(..mark.len());
                self.buf_at = mark.len() as u64;
            }
            self.bom_done = true;
        }
        let mut start = 0usize;
        match c.encoding {
            Encoding::Utf8 => {
                while let Some(i) = self.buf.get(start..).and_then(|b| memchr::memchr(b'\n', b)) {
                    let end = start + i;
                    let mut line = self.buf.get(start..end).unwrap_or_default();
                    if let Some(l) = line.strip_suffix(b"\r") {
                        line = l;
                    }
                    let text = String::from_utf8_lossy(line).into_owned();
                    let at = self.buf_at + end as u64 + 1;
                    self.line(c, sink, text, at, false);
                    start = end + 1;
                }
            }
            Encoding::Utf16Le | Encoding::Utf16Be => {
                let nl: [u8; 2] = if c.encoding == Encoding::Utf16Le { [0x0A, 0] } else { [0, 0x0A] };
                let mut j = 0;
                while j + 1 < self.buf.len() {
                    if self.buf.get(j..j + 2) == Some(&nl[..]) {
                        let mut text = decode(self.buf.get(start..j).unwrap_or_default(), c.encoding);
                        if text.ends_with('\r') {
                            text.pop();
                        }
                        let at = self.buf_at + j as u64 + 2;
                        self.line(c, sink, text, at, false);
                        start = j + 2;
                    }
                    j += 2;
                }
            }
        }
        self.buf.drain(..start);
        self.buf_at += start as u64;
        while self.buf.len() >= MAX_LINE || (flush && !self.buf.is_empty()) {
            let cut = boundary(&self.buf, MAX_LINE, c.encoding).max(1);
            let mut text = decode(self.buf.get(..cut).unwrap_or_default(), c.encoding);
            if text.ends_with('\r') {
                text.pop();
            }
            let at = self.buf_at + cut as u64;
            let cut_short = self.buf.len() >= MAX_LINE;
            self.line(c, sink, text, at, cut_short);
            self.buf.drain(..cut);
            self.buf_at += cut as u64;
        }
    }

    /// A line as read, through its format: a part of a longer line waits for the rest.
    fn line(&mut self, c: &Compiled, sink: &mut Sink, text: String, end: u64, cut: bool) {
        let whole = match c.format {
            Format::Cri => match cri(&text) {
                Some((ts, stream, partial, msg)) => {
                    let p = self.part.get_or_insert_with(|| Line { ts, stream: Some(stream), ..Line::default() });
                    if p.text.len() + msg.len() <= MAX_LINE {
                        p.text.push_str(msg);
                    } else {
                        p.truncated = true;
                    }
                    if partial {
                        return;
                    }
                    self.part.take().unwrap_or_default()
                }
                None => Line { text, truncated: cut, ..Line::default() },
            },
            Format::Docker => match docker(&text) {
                Some((ts, stream, partial, log)) => {
                    let p = self.part.get_or_insert_with(|| Line { ts, stream: Some(stream), ..Line::default() });
                    if p.text.len() + log.len() <= MAX_LINE {
                        p.text.push_str(&log);
                    } else {
                        p.truncated = true;
                    }
                    if partial {
                        return;
                    }
                    self.part.take().unwrap_or_default()
                }
                None => Line { text, truncated: cut, ..Line::default() },
            },
            Format::Auto | Format::Text | Format::Json => Line { text, truncated: cut, ..Line::default() },
        };
        self.complete(c, sink, whole, end);
    }

    /// A whole line: a record, or a line of the record a `multi_line` rule is making.
    fn complete(&mut self, c: &Compiled, sink: &mut Sink, line: Line, end: u64) {
        let Some(re) = &c.multiline else {
            self.emit(c, sink, line, end);
            return;
        };
        let starts = re.is_match(&line.text);
        match &mut self.multi {
            Some((m, at, since)) if !starts && m.text.len() + 1 + line.text.len() <= MAX_LINE => {
                m.text.push('\n');
                m.text.push_str(&line.text);
                m.truncated |= line.truncated;
                *at = end;
                *since = Instant::now();
            }
            _ => {
                // A record starts here: the one before is whole (or as long as a record holds).
                if let Some((mut m, at, _)) = self.multi.take() {
                    m.truncated |= !starts;
                    self.emit(c, sink, m, at);
                }
                self.multi = Some((line, end, Instant::now()));
            }
        }
    }

    /// A record through the rules into the batch, its mark there whether it is kept or not.
    fn emit(&mut self, c: &Compiled, sink: &mut Sink, mut line: Line, end: u64) {
        if line.truncated {
            sink.truncated += 1;
        }
        if !line.text.trim().is_empty() {
            if c.keep(&mut line.text) {
                sink.batch.push(c.record(line, &self.origin, sink.now_ns));
            } else {
                sink.excluded += 1;
            }
        }
        mark(&mut sink.batch, (self.key, self.generation, end));
        self.emitted = self.emitted.max(end);
    }

    /// What waits on time: a multi-line record no line was added to for [`MULTI_WAIT`], a last
    /// line without its newline (and a long line's parts) once the file has not grown for
    /// [`PARTIAL_WAIT`]; with `all`, everything (the file is closing).
    fn timers(&mut self, c: &Compiled, sink: &mut Sink, all: bool) {
        let idle = all || self.grew.elapsed() >= PARTIAL_WAIT;
        if idle && !self.buf.is_empty() {
            self.lines(c, sink, true);
        }
        if idle && let Some(p) = self.part.take() {
            self.complete(c, sink, p, self.buf_at);
        }
        if let Some((_, _, since)) = &self.multi
            && (all || since.elapsed() >= MULTI_WAIT)
            && let Some((m, at, _)) = self.multi.take()
        {
            self.emit(c, sink, m, at);
        }
    }
}

/// The hash of `f`'s first `n` bytes (its place kept), read without moving it.
fn head_hash(f: &mut File, n: usize) -> std::io::Result<u64> {
    let at = f.stream_position()?;
    f.seek(SeekFrom::Start(0))?;
    let mut b = vec![0u8; n];
    let mut got = 0;
    while got < n {
        match b.get_mut(got..).map(|s| f.read(s)) {
            Some(Ok(0)) | None => break,
            Some(Ok(k)) => got += k,
            Some(Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Some(Err(e)) => return Err(e),
        }
    }
    f.seek(SeekFrom::Start(at))?;
    Ok(fnv(b.get(..got).unwrap_or_default()))
}

/// Where the line holding the file's last byte starts (a file read from its end starts at a line):
/// looked for in its last 64 KiB, else its end.
fn last_line_start(f: &mut File, size: u64, e: Encoding) -> std::io::Result<u64> {
    if e != Encoding::Utf8 {
        return Ok(size & !1);
    }
    let from = size.saturating_sub(64 << 10);
    f.seek(SeekFrom::Start(from))?;
    let mut b = Vec::new();
    f.take(size - from).read_to_end(&mut b)?;
    Ok(memchr::memrchr(b'\n', &b).map_or(if from == 0 { 0 } else { size }, |i| from + i as u64 + 1))
}

/// A Kubernetes pod's container log's path (`/var/log/pods/<namespace>_<pod>_<uid>/<container>/
/// <restarts>.log`): what it names.
fn pod_of(path: &Path) -> Option<Vec<(String, String)>> {
    let restarts = path.file_stem()?.to_str()?;
    let container = path.parent()?.file_name()?.to_str()?;
    let pod_dir = path.parent()?.parent()?.file_name()?.to_str()?;
    let mut p = pod_dir.splitn(3, '_');
    let (ns, pod, uid) = (p.next()?, p.next()?, p.next()?);
    Some(vec![
        ("k8s.namespace.name".into(), ns.into()),
        ("k8s.pod.name".into(), pod.into()),
        ("k8s.pod.uid".into(), uid.into()),
        ("k8s.container.name".into(), container.into()),
        ("k8s.container.restart_count".into(), restarts.into()),
    ])
}

/// A Docker container's log's path (`<root>/<id>/<id>-json.log`): its id, and from its
/// `config.v2.json` (when this process may read it) its name, image and Compose service.
fn container_of(path: &Path) -> Option<(Vec<(String, String)>, Option<String>)> {
    let dir = path.parent()?;
    let id = dir.file_name()?.to_str()?.to_owned();
    let mut attrs = vec![("container.id".to_owned(), id)];
    let mut service = None;
    let config = File::open(dir.join("config.v2.json")).ok().and_then(|f| {
        let mut b = Vec::new();
        f.take(CONFIG_MAX).read_to_end(&mut b).ok()?;
        serde_json::from_slice::<serde_json::Value>(&b).ok()
    });
    if let Some(v) = config {
        let name = v.get("Name").and_then(|n| n.as_str()).map(|n| n.trim_start_matches('/').to_owned()).filter(|n| !n.is_empty());
        if let Some(image) = v.pointer("/Config/Image").and_then(|i| i.as_str()) {
            attrs.push(("container.image.name".into(), image.into()));
        }
        service = v.pointer("/Config/Labels/com.docker.compose.service").and_then(|s| s.as_str()).map(str::to_owned).or_else(|| name.clone());
        if let Some(n) = name {
            attrs.push(("container.name".into(), n));
        }
    }
    Some((attrs, service))
}

fn origin(c: &Compiled, path: &Path) -> Origin {
    let mut attrs = vec![("log.file.path".to_owned(), path.display().to_string())];
    let mut service = None;
    match c.kind {
        Kind::Kubernetes => {
            if let Some(a) = pod_of(path) {
                service = a.iter().find(|(k, _)| k == "k8s.container.name").map(|(_, v)| v.clone());
                attrs.extend(a);
            }
        }
        Kind::Docker => {
            if let Some((a, s)) = container_of(path) {
                attrs.extend(a);
                service = s;
            }
        }
        Kind::File | Kind::Journald | Kind::Syslog => {}
    }
    Origin { attrs, service }
}

/// The files a source's globs match, less those it leaves out ([`MATCH_MAX`] at the most), and the
/// last error met on the way (a folder it may not read).
fn matched(c: &Compiled) -> (Vec<(PathBuf, std::fs::Metadata)>, Option<String>) {
    let opts = glob::MatchOptions { case_sensitive: true, require_literal_separator: true, require_literal_leading_dot: false };
    let mut found: Vec<(PathBuf, std::fs::Metadata)> = Vec::new();
    let mut paths: HashSet<PathBuf> = HashSet::new();
    let mut error = None;
    'globs: for g in &c.globs {
        let matched = match glob::glob_with(g, opts) {
            Ok(m) => m,
            Err(e) => {
                error = Some(format!("{g}: {e}"));
                continue;
            }
        };
        for p in matched {
            match p {
                Ok(path) => {
                    if found.len() >= MATCH_MAX {
                        break 'globs;
                    }
                    if c.exclude.iter().any(|x| x.matches_path_with(&path, opts)) || !paths.insert(path.clone()) {
                        continue;
                    }
                    match std::fs::metadata(&path) {
                        Ok(m) if m.is_file() => found.push((path, m)),
                        Ok(_) => {}
                        Err(e) => error = Some(format!("{}: {e}", path.display())),
                    }
                }
                // A folder on the way it may not read.
                Err(e) => error = Some(e.to_string()),
            }
        }
    }
    (found, error)
}

/// What a source would read here now, nothing written.
pub(crate) struct Sampled {
    /// The files it matches, the newest first (50 of them), with their sizes; how many in all.
    pub files: Vec<(String, u64)>,
    pub matched: usize,
    /// The records of the newest file's last lines (the journal's last entries), the newest last.
    pub records: Vec<kiln_obs::logs::record::Record>,
    pub excluded: u64,
    pub error: Option<String>,
}

/// The files `c` matches, and the records of the last 256 KiB of the newest (from its first whole
/// line), `n` of them at the most, read as the source reads them.
pub(crate) fn sample(c: &Compiled, n: usize) -> Sampled {
    let (mut found, error) = matched(c);
    found.sort_by_key(|(_, m)| Reverse(m.modified().ok()));
    let mut out = Sampled { files: found.iter().take(50).map(|(p, m)| (p.display().to_string(), m.len())).collect(), matched: found.len(), records: Vec::new(), excluded: 0, error };
    let Some((path, m)) = found.first() else { return out };
    let read = (|| -> std::io::Result<Sink> {
        let mut file = File::open(path)?;
        let size = m.len();
        let mut from = size.saturating_sub(256 << 10);
        if from > 0 {
            // The first whole line.
            file.seek(SeekFrom::Start(from))?;
            let mut b = vec![0u8; 64 << 10];
            let k = file.read(&mut b)?;
            from += memchr::memchr(b'\n', b.get(..k).unwrap_or_default()).map_or(0, |i| i as u64 + 1);
            if c.encoding != Encoding::Utf8 {
                from &= !1;
            }
        }
        file.seek(SeekFrom::Start(from))?;
        let now = Instant::now();
        let mut t = Tail { key: Key(0, 0), origin: origin(c, path), path: path.clone(), file, generation: 0, read: from, size, emitted: from, buf: Vec::new(), buf_at: from, grew: now, looked: now, gone: false, part: None, multi: None, bom_done: from != 0 };
        let mut sink = Sink { batch: Batch::default(), excluded: 0, truncated: 0, now_ns: crate::observe::clock_ms().saturating_mul(1_000_000) };
        let mut scratch = vec![0u8; READ];
        let mut budget = (size - from) as usize + READ;
        while t.read(c, &mut sink, &mut scratch, &mut budget)? > 0 {}
        t.timers(c, &mut sink, true);
        Ok(sink)
    })();
    match read {
        Ok(mut sink) => {
            let skip = sink.batch.records.len().saturating_sub(n);
            out.records = sink.batch.records.split_off(skip);
            out.excluded = sink.excluded;
        }
        Err(e) => out.error = Some(format!("{}: {e}", path.display())),
    }
    out
}

/// A file that was truncated: the hash of its leading `len` bytes and how far it had been read,
/// for its copy.
struct Former {
    head: u64,
    len: u32,
    offset: u64,
    at: Instant,
}

/// The fewest leading bytes a truncated file's copy is known by: fewer could be any file's.
const FORMER_MIN: u32 = 16;

/// A source's files.
struct Files<'a> {
    c: &'a Compiled,
    saved_to: PathBuf,
    tails: Vec<Tail>,
    places: HashMap<Key, Place>,
    formers: Vec<Former>,
    flight: InFlight<Mark>,
    sink: Sink,
    scratch: Vec<u8>,
    saved: Instant,
    dirty: bool,
    bytes: u64,
    records: u64,
    over_limit: u64,
    last_ms: Option<i64>,
    error: Option<String>,
    errors: u64,
}

impl<'a> Files<'a> {
    fn new(c: &'a Compiled, saved_to: &Path) -> Self {
        let saved: Saved = std::fs::read(saved_to).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Self {
            c,
            saved_to: saved_to.to_owned(),
            tails: Vec::new(),
            places: saved.places.into_iter().map(|p| (p.key, p)).collect(),
            formers: Vec::new(),
            flight: InFlight::default(),
            sink: Sink { batch: Batch::default(), excluded: 0, truncated: 0, now_ns: 0 },
            scratch: vec![0; READ],
            saved: Instant::now(),
            dirty: false,
            bytes: 0,
            records: 0,
            over_limit: 0,
            last_ms: None,
            error: None,
            errors: 0,
        }
    }

    fn fail(&mut self, e: String) {
        self.errors += 1;
        self.error = Some(e);
    }

    /// The paths looked at: new files opened, renamed ones followed, those gone noted.
    fn scan(&mut self, first: bool) {
        let (found, mut error) = matched(self.c);
        let mut here: HashSet<Key> = HashSet::new();
        let mut new = Vec::new();
        for (path, m) in found {
            let key = key_of(&m, &path);
            if !here.insert(key) {
                continue;
            }
            match self.tails.iter_mut().find(|t| t.key == key) {
                Some(t) => {
                    if t.path != path {
                        t.origin = origin(self.c, &path);
                        t.path = path;
                    }
                    t.gone = false;
                }
                None => new.push((path, m, key)),
            }
        }
        let now_ms = crate::observe::clock_ms();
        for i in 0..self.tails.len() {
            let Some(t) = self.tails.get_mut(i) else { continue };
            if !here.contains(&t.key) {
                t.gone = true;
                continue;
            }
            // Its leading bytes: what they were, or it was rewritten; more of them taken as it grows,
            // to [`HEAD`].
            let (key, size) = (t.key, t.file.metadata().map_or(t.size, |m| m.len()));
            t.size = size;
            let Some(p) = self.places.get_mut(&key) else { continue };
            p.seen = now_ms;
            p.path = t.path.display().to_string();
            let had = p.head_len as usize;
            let same = match had {
                0 => Ok(true),
                _ if size < had as u64 => Ok(false),
                _ => head_hash(&mut t.file, had).map(|h| h == p.head),
            };
            match same {
                Ok(false) => {
                    self.formers.push(Former { head: p.head, len: p.head_len, offset: t.emitted, at: Instant::now() });
                    if t.rewound().is_ok() {
                        p.generation = t.generation;
                        p.offset = 0;
                        p.head = 0;
                        p.head_len = 0;
                        self.dirty = true;
                    }
                }
                Ok(true) if had < HEAD && size > had as u64 => {
                    let len = HEAD.min(usize::try_from(size).unwrap_or(usize::MAX));
                    match head_hash(&mut t.file, len) {
                        Ok(h) => {
                            p.head = h;
                            p.head_len = u32::try_from(len).unwrap_or(u32::MAX);
                            self.dirty = true;
                        }
                        Err(e) => error = Some(format!("{}: {e}", t.path.display())),
                    }
                }
                Ok(true) => {}
                Err(e) => error = Some(format!("{}: {e}", t.path.display())),
            }
        }
        self.formers.retain(|f| f.at.elapsed() < FORMER_WAIT);
        if self.formers.len() > 64 {
            self.formers.drain(..self.formers.len() - 64);
        }
        new.sort_by_key(|(_, m, _)| Reverse(m.modified().ok()));
        let room = OPEN_MAX.saturating_sub(self.tails.len());
        self.over_limit = new.len().saturating_sub(room) as u64;
        for (path, m, key) in new.into_iter().take(room) {
            if let Err(e) = self.open(path.clone(), &m, key, first) {
                error = Some(format!("{}: {e}", path.display()));
            }
        }
        match error {
            Some(e) => self.fail(e),
            None => self.error = None,
        }
    }

    /// Opens a file found: read from its place if it has one and is the same file; else from its
    /// original's place if it is a copy; else from its start (its end, at the source's start,
    /// with `start: end`).
    fn open(&mut self, path: PathBuf, m: &std::fs::Metadata, key: Key, first: bool) -> std::io::Result<()> {
        let mut file = File::open(&path)?;
        let size = m.len();
        let head_len = HEAD.min(usize::try_from(size).unwrap_or(usize::MAX));
        let head = head_hash(&mut file, head_len)?;
        let (offset, generation) = match self.places.get(&key) {
            Some(p) if (p.head_len as u64) <= size && (p.head_len == 0 || head_hash(&mut file, p.head_len as usize)? == p.head) => {
                if p.offset > size { (0, p.generation.wrapping_add(1)) } else { (p.offset, p.generation) }
            }
            // Another file under an identity reused.
            Some(p) => (0, p.generation.wrapping_add(1)),
            None => {
                // A copy: its leading bytes a file's read here (open, kept, or truncated a moment
                // ago: logrotate's copytruncate copies, then truncates).
                let mut copy_of = (head_len == HEAD)
                    .then(|| {
                        self.tails
                            .iter()
                            .find(|t| t.key != key && self.places.get(&t.key).is_some_and(|p| p.head_len as usize == HEAD && p.head == head))
                            .map(|t| t.emitted)
                            .or_else(|| self.places.values().find(|p| p.key != key && p.head_len as usize == HEAD && p.head == head).map(|p| p.offset))
                    })
                    .flatten();
                if copy_of.is_none() {
                    for f in self.formers.iter().rev().filter(|f| f.len >= FORMER_MIN && u64::from(f.len) <= size) {
                        if head_hash(&mut file, f.len as usize)? == f.head {
                            copy_of = Some(f.offset);
                            break;
                        }
                    }
                }
                match copy_of {
                    Some(o) => (o.min(size), 0),
                    None if first && self.c.start == Start::End => (last_line_start(&mut file, size, self.c.encoding)?, 0),
                    None => (0, 0),
                }
            }
        };
        file.seek(SeekFrom::Start(offset))?;
        self.places.insert(key, Place { key, path: path.display().to_string(), offset, head, head_len: u32::try_from(head_len).unwrap_or(u32::MAX), generation, seen: crate::observe::clock_ms() });
        self.dirty = true;
        let now = Instant::now();
        self.tails.push(Tail {
            key,
            origin: origin(self.c, &path),
            path,
            file,
            generation,
            read: offset,
            size,
            emitted: offset,
            buf: Vec::new(),
            buf_at: offset,
            grew: now,
            looked: now,
            gone: false,
            part: None,
            multi: None,
            bom_done: offset != 0,
        });
        Ok(())
    }

    /// A round: what each file has read and handed on, offered; whether any file had more.
    fn round(&mut self, offer: Offer<'_>) -> bool {
        // An offer refused before goes first: nothing more is read until it is taken.
        if !self.sink.batch.is_empty() && !self.flight.offer(&mut self.sink.batch, offer) {
            return false;
        }
        self.sink.now_ns = crate::observe::clock_ms().saturating_mul(1_000_000);
        let mut busy = false;
        let mut i = 0;
        while i < self.tails.len() {
            let Some(t) = self.tails.get_mut(i) else { break };
            let mut budget = ROUND_BYTES;
            let read = t.read(self.c, &mut self.sink, &mut self.scratch, &mut budget);
            match read {
                Ok(0) => match t.truncated() {
                    Ok(true) => {
                        let (head, len) = self.places.get(&t.key).map_or((0, 0), |p| (p.head, p.head_len));
                        self.formers.push(Former { head, len, offset: t.emitted, at: Instant::now() });
                        if t.rewound().is_ok()
                            && let Some(p) = self.places.get_mut(&t.key)
                        {
                            p.generation = t.generation;
                            p.offset = 0;
                            p.head = 0;
                            p.head_len = 0;
                            self.dirty = true;
                        }
                        busy = true;
                    }
                    Ok(false) => {}
                    Err(e) => {
                        let why = format!("{}: {e}", t.path.display());
                        self.fail(why);
                    }
                },
                Ok(n) => {
                    self.bytes += n as u64;
                    busy |= budget == 0;
                }
                Err(e) => {
                    let why = format!("{}: {e}", t.path.display());
                    self.fail(why);
                }
            }
            let Some(t) = self.tails.get_mut(i) else { break };
            let closing = t.gone && t.grew.elapsed() >= GONE_WAIT;
            t.timers(self.c, &mut self.sink, closing);
            if closing {
                self.tails.remove(i);
            } else {
                i += 1;
            }
            if self.sink.batch.full() && !self.flight.offer(&mut self.sink.batch, offer) {
                return true;
            }
        }
        let _ = self.flight.offer(&mut self.sink.batch, offer);
        busy
    }

    /// Places moved past the records written.
    fn written(&mut self) {
        let (places, mut moved, mut records) = (&mut self.places, false, 0);
        self.flight.written(|marks, n| {
            for (key, generation, end) in marks {
                if let Some(p) = places.get_mut(&key)
                    && p.generation == generation
                    && end > p.offset
                {
                    p.offset = end;
                    moved = true;
                }
            }
            records += n;
        });
        if records > 0 {
            self.records += records;
            self.last_ms = Some(crate::observe::clock_ms());
        }
        self.dirty |= moved;
    }

    /// The places written down, once a [`SAVE_EVERY`] at the most (`now`: whatever the time).
    fn save(&mut self, now: bool) {
        if !self.dirty || (!now && self.saved.elapsed() < SAVE_EVERY) {
            return;
        }
        let open: HashSet<Key> = self.tails.iter().map(|t| t.key).collect();
        let cutoff = crate::observe::clock_ms() - FORGET_MS;
        self.places.retain(|k, p| open.contains(k) || p.seen >= cutoff);
        let mut places: Vec<Place> = self.places.values().cloned().collect();
        if places.len() > PLACES_MAX {
            places.sort_by_key(|p| Reverse((open.contains(&p.key), p.seen)));
            places.truncate(PLACES_MAX);
            self.places = places.iter().map(|p| (p.key, p.clone())).collect();
        }
        match serde_json::to_vec(&Saved { places }).map_err(std::io::Error::other).and_then(|b| crate::sync::group::save_atomic(&self.saved_to, &b)) {
            Ok(()) => {
                self.dirty = false;
                self.saved = Instant::now();
            }
            Err(e) => {
                let why = format!("its places could not be written down: {e}");
                self.fail(why);
            }
        }
    }

    fn report(&self, state: &Mutex<State>) {
        let lag = self.tails.iter().map(|t| self.places.get(&t.key).filter(|p| p.generation == t.generation).map_or(t.size, |p| t.size.saturating_sub(p.offset))).sum();
        let mut s = super::lock(state);
        s.files = self.tails.len() as u64;
        s.over_limit = self.over_limit;
        s.bytes = self.bytes;
        s.records = self.records;
        s.in_flight = self.flight.records();
        s.excluded = self.sink.excluded;
        s.truncated = self.sink.truncated;
        s.lag_bytes = lag;
        s.last_ms = self.last_ms;
        s.error.clone_from(&self.error);
        s.errors = self.errors;
    }
}

/// Every file `c` matches read whole on this thread, each offer written at once (a bench's): the
/// records and the bytes read.
pub(crate) fn read_whole(c: &Compiled, saved_to: &Path) -> (u64, u64) {
    let records = std::sync::atomic::AtomicU64::new(0);
    let offer = |r: Vec<kiln_obs::logs::record::Record>, a: crate::observe::ingest::Ack| {
        records.fetch_add(r.len() as u64, std::sync::atomic::Ordering::Relaxed);
        drop(r);
        a.set();
        Ok(())
    };
    let mut f = Files::new(c, saved_to);
    f.scan(true);
    while f.round(&offer) {
        f.written();
    }
    for t in &mut f.tails {
        t.timers(c, &mut f.sink, true);
    }
    let _ = f.flight.offer(&mut f.sink.batch, &offer);
    f.written();
    (records.load(std::sync::atomic::Ordering::Relaxed), f.bytes)
}

/// A file source's thread: until told to stop, its globs looked at every [`SCAN`] and its files
/// read; then what is in flight given a moment to be written, and its places written down.
pub(crate) fn run(c: &Compiled, saved_to: &Path, stop: &Stop, state: &Mutex<State>, offer: Offer<'_>) {
    let mut f = Files::new(c, saved_to);
    let mut scanned: Option<Instant> = None;
    while !stop.stopped() {
        if scanned.is_none_or(|t| t.elapsed() >= SCAN) {
            f.scan(scanned.is_none());
            scanned = Some(Instant::now());
        }
        let busy = f.round(offer);
        f.written();
        f.save(false);
        f.report(state);
        if !busy {
            stop.sleep(POLL);
        }
    }
    let waiting = f.flight.records();
    let (places, mut records) = (&mut f.places, 0);
    f.flight.drain(|marks, n| {
        for (key, generation, end) in marks {
            if let Some(p) = places.get_mut(&key)
                && p.generation == generation
                && end > p.offset
            {
                p.offset = end;
            }
        }
        records += n;
    });
    f.records += records;
    f.dirty = true;
    f.save(true);
    f.report(state);
    tracing::info!(source = %c.name, in_flight = waiting, written_since = records, not_written = f.flight.records(), saved = !f.dirty, "log source stopped: its places written down");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::collect::{Rule, RuleKind, Source};
    use crate::observe::ingest::Ack;
    use kiln_obs::logs::record::Record;
    use std::io::Write as _;
    use std::sync::Arc;

    /// What a source wrote: records whose acks were given, and those whose acks are held back
    /// (a write not yet applied), to give later.
    #[derive(Default)]
    struct Sent {
        written: Mutex<Vec<Record>>,
        held: Mutex<Vec<(Vec<Record>, Ack)>>,
        hold: std::sync::atomic::AtomicBool,
        refuse: std::sync::atomic::AtomicBool,
    }

    impl Sent {
        fn offer(&self, records: Vec<Record>, ack: Ack) -> Result<(), Vec<Record>> {
            use std::sync::atomic::Ordering;
            if self.refuse.load(Ordering::SeqCst) {
                return Err(records);
            }
            if self.hold.load(Ordering::SeqCst) {
                self.held.lock().unwrap_or_else(PoisonError::into_inner).push((records, ack));
            } else {
                self.written.lock().unwrap_or_else(PoisonError::into_inner).extend(records);
                ack.set();
            }
            Ok(())
        }

        fn messages(&self) -> Vec<String> {
            self.written.lock().unwrap_or_else(PoisonError::into_inner).iter().map(|r| r.message.clone()).collect()
        }
    }

    use std::sync::PoisonError;

    fn source(dir: &Path, rules: Vec<Rule>) -> Source {
        Source {
            kind: Kind::File,
            paths: vec![format!("{}/*.log", dir.display())],
            exclude: vec![format!("{}/skip*.log", dir.display())],
            format: Format::Auto,
            start: Start::Beginning,
            encoding: Encoding::Utf8,
            units: Vec::new(),
            listen: Vec::new(),
            timezone: None,
            service: Some("app".into()),
            source: None,
            tags: Vec::new(),
            rules,
            on: "server".into(),
        }
    }

    /// Steps a source's files by hand: a scan when asked, rounds until nothing more is read.
    fn step(f: &mut Files<'_>, sent: &Arc<Sent>, scan: bool) {
        let s = sent.clone();
        let offer = move |r: Vec<Record>, a: Ack| s.offer(r, a);
        if scan {
            f.scan(false);
        }
        for _ in 0..100 {
            let busy = f.round(&offer);
            f.written();
            if !busy {
                break;
            }
        }
    }

    fn append(path: &Path, text: &str) -> std::io::Result<()> {
        std::fs::OpenOptions::new().create(true).append(true).open(path)?.write_all(text.as_bytes())
    }

    #[test]
    fn lines_are_read_once_each_through_rotation_truncation_and_a_restart() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let (dir, saved) = (tmp.path().join("logs"), tmp.path().join("places.json"));
        std::fs::create_dir_all(&dir)?;
        let c = source(&dir, Vec::new()).compile("app", "h").map_err(anyhow::Error::msg)?;
        let sent = Arc::new(Sent::default());
        let log = dir.join("app.log");
        append(&log, "one\ntwo\r\nthree")?;
        append(&dir.join("skip-me.log"), "never\n")?;
        let mut f = Files::new(&c, &saved);
        f.scan(true);
        step(&mut f, &sent, false);
        anyhow::ensure!(sent.messages() == ["one", "two"], "{:?}", sent.messages());
        // The line without its newline, finished.
        append(&log, " and a half\nfour\n")?;
        step(&mut f, &sent, false);
        anyhow::ensure!(sent.messages() == ["one", "two", "three and a half", "four"], "{:?}", sent.messages());
        // Renamed (logrotate's create): the old file read to its end, the new one from its start.
        std::fs::rename(&log, dir.join("app.log.1"))?;
        append(&dir.join("app.log.1"), "five, late, to the old file\n")?;
        append(&log, "six, in the new file\n")?;
        step(&mut f, &sent, true);
        anyhow::ensure!(sent.messages().len() == 6 && sent.messages().contains(&"five, late, to the old file".to_owned()) && sent.messages().contains(&"six, in the new file".to_owned()), "{:?}", sent.messages());
        // Truncated (copytruncate): its copy is not read again; the file from its start.
        let copy = dir.join("app-copy.log");
        std::fs::copy(&log, &copy)?;
        std::fs::OpenOptions::new().write(true).truncate(true).open(&log)?;
        f.tails.iter_mut().for_each(|t| t.looked = Instant::now() - LOOK);
        append(&log, "seven, after the truncation\n")?;
        step(&mut f, &sent, true);
        step(&mut f, &sent, true);
        let m = sent.messages();
        anyhow::ensure!(m.len() == 7 && m.last().map(String::as_str) == Some("seven, after the truncation"), "{m:?}");
        // Stopped and started again: nothing repeated, nothing lost.
        f.save(true);
        drop(f);
        append(&log, "eight, while it was away\n")?;
        let mut f = Files::new(&c, &saved);
        f.scan(true);
        step(&mut f, &sent, false);
        let m = sent.messages();
        anyhow::ensure!(m.len() == 8 && m.last().map(String::as_str) == Some("eight, while it was away"), "{m:?}");
        Ok(())
    }

    #[test]
    fn a_place_moves_only_past_records_written_so_a_crash_repeats_and_never_loses() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let (dir, saved) = (tmp.path().join("logs"), tmp.path().join("places.json"));
        std::fs::create_dir_all(&dir)?;
        let c = source(&dir, Vec::new()).compile("app", "h").map_err(anyhow::Error::msg)?;
        let sent = Arc::new(Sent::default());
        let log = dir.join("app.log");
        append(&log, "a\nb\n")?;
        let mut f = Files::new(&c, &saved);
        f.scan(true);
        step(&mut f, &sent, false);
        f.save(true);
        // The next lines offered, their write not yet applied, when the process dies.
        sent.hold.store(true, std::sync::atomic::Ordering::SeqCst);
        append(&log, "c\nd\n")?;
        step(&mut f, &sent, false);
        f.save(true);
        drop(f);
        // Started again: c and d are read again (their write may not have been applied).
        sent.hold.store(false, std::sync::atomic::Ordering::SeqCst);
        let mut f = Files::new(&c, &saved);
        f.scan(true);
        step(&mut f, &sent, false);
        anyhow::ensure!(sent.messages() == ["a", "b", "c", "d"], "{:?}", sent.messages());
        // Refused (no room): nothing is lost, and nothing more read until it is taken.
        sent.refuse.store(true, std::sync::atomic::Ordering::SeqCst);
        append(&log, "e\n")?;
        step(&mut f, &sent, false);
        anyhow::ensure!(sent.messages().len() == 4);
        sent.refuse.store(false, std::sync::atomic::Ordering::SeqCst);
        step(&mut f, &sent, false);
        anyhow::ensure!(sent.messages() == ["a", "b", "c", "d", "e"], "{:?}", sent.messages());
        Ok(())
    }

    #[test]
    fn container_lines_join_their_parts_and_a_multi_line_record_is_one() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        // Kubernetes' layout.
        let pod = tmp.path().join("pods/shop_web-7d4b9c_0f2c/nginx");
        std::fs::create_dir_all(&pod)?;
        let k = Source { kind: Kind::Kubernetes, paths: vec![format!("{}/pods/*/*/*.log", tmp.path().display())], exclude: Vec::new(), service: None, ..source(tmp.path(), Vec::new()) };
        let c = k.compile("pods", "node-1").map_err(anyhow::Error::msg)?;
        append(&pod.join("0.log"), "2026-10-18T12:00:00.000000001Z stdout P first half, \n2026-10-18T12:00:00.000000002Z stdout F second half\n2026-10-18T12:00:01Z stderr F {\"level\":\"error\",\"msg\":\"boom\"}\n")?;
        let sent = Arc::new(Sent::default());
        let mut f = Files::new(&c, &tmp.path().join("k.json"));
        f.scan(true);
        step(&mut f, &sent, false);
        let w = sent.written.lock().unwrap_or_else(PoisonError::into_inner).clone();
        anyhow::ensure!(w.len() == 2 && w.first().is_some_and(|r| r.message == "first half, second half" && r.ts == 1_792_324_800_000_000_001 && r.service == "nginx"), "{w:?}");
        let attr = |r: &Record, k: &str| r.attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
        anyhow::ensure!(w.get(1).is_some_and(|r| r.message == "boom" && r.severity == 17 && attr(r, "log.iostream").as_deref() == Some("stderr") && attr(r, "k8s.pod.name").as_deref() == Some("web-7d4b9c") && attr(r, "k8s.namespace.name").as_deref() == Some("shop")), "{w:?}");
        // Docker's json-file, and a Java stack trace as one record.
        let d = tmp.path().join("docker/abc123");
        std::fs::create_dir_all(&d)?;
        std::fs::write(d.join("config.v2.json"), r#"{"Name":"/shop-web-1","Config":{"Image":"nginx:1.27","Labels":{"com.docker.compose.service":"web"}}}"#)?;
        let rule = Rule { kind: RuleKind::MultiLine, name: String::new(), pattern: "^\\d{4}-\\d{2}-\\d{2}".into(), replace_placeholder: None };
        let ds = Source { kind: Kind::Docker, paths: vec![format!("{}/docker/*/*-json.log", tmp.path().display())], exclude: Vec::new(), service: None, rules: vec![rule], ..source(tmp.path(), Vec::new()) };
        let c = ds.compile("docker", "node-1").map_err(anyhow::Error::msg)?;
        let lines = [
            "2026-10-18 12:00:00 ERROR request failed\n",
            "java.lang.IllegalStateException: no\n",
            "\tat Shop.pay(Shop.java:42)\n",
            "2026-10-18 12:00:01 INFO next\n",
        ];
        let body: String = lines.iter().map(|l| format!("{}\n", serde_json::json!({ "log": l, "stream": "stdout", "time": "2026-10-18T12:00:00Z" }))).collect();
        append(&d.join("abc123-json.log"), &body)?;
        let sent = Arc::new(Sent::default());
        let mut f = Files::new(&c, &tmp.path().join("d.json"));
        f.scan(true);
        step(&mut f, &sent, false);
        // The last record waits for its next line, or the wait.
        f.tails.iter_mut().for_each(|t| t.multi.iter_mut().for_each(|m| m.2 = Instant::now() - MULTI_WAIT));
        step(&mut f, &sent, false);
        let w = sent.written.lock().unwrap_or_else(PoisonError::into_inner).clone();
        anyhow::ensure!(w.len() == 2, "{w:?}");
        anyhow::ensure!(w.first().is_some_and(|r| r.message == "2026-10-18 12:00:00 ERROR request failed\njava.lang.IllegalStateException: no\n\tat Shop.pay(Shop.java:42)" && r.severity == 17 && r.service == "web" && attr(r, "container.name").as_deref() == Some("shop-web-1")), "{w:?}");
        Ok(())
    }

    #[test]
    fn a_long_line_is_cut_a_partial_one_waits_and_utf16_reads() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let dir = tmp.path().join("logs");
        std::fs::create_dir_all(&dir)?;
        let c = source(&dir, Vec::new()).compile("app", "h").map_err(anyhow::Error::msg)?;
        let sent = Arc::new(Sent::default());
        let long = "é".repeat(MAX_LINE / 2 + 10);
        append(&dir.join("app.log"), &format!("{long}\nwaiting"))?;
        let mut f = Files::new(&c, &tmp.path().join("p.json"));
        f.scan(true);
        step(&mut f, &sent, false);
        let w = sent.written.lock().unwrap_or_else(PoisonError::into_inner).clone();
        anyhow::ensure!(w.len() == 2 && w.iter().map(|r| r.message.chars().count()).sum::<usize>() == MAX_LINE / 2 + 10 && w.first().is_some_and(|r| r.attrs.iter().any(|(k, _)| k == "truncated")), "{}", w.len());
        f.tails.iter_mut().for_each(|t| t.grew = Instant::now() - PARTIAL_WAIT);
        step(&mut f, &sent, false);
        anyhow::ensure!(sent.messages().last().map(String::as_str) == Some("waiting"));
        // UTF-16, its byte order mark dropped.
        let u = Source { encoding: Encoding::Utf16Le, paths: vec![format!("{}/*.u16", dir.display())], ..source(&dir, Vec::new()) };
        let c = u.compile("u", "h").map_err(anyhow::Error::msg)?;
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "héllo wörld\r\nnext\n".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        std::fs::write(dir.join("w.u16"), &bytes)?;
        let sent = Arc::new(Sent::default());
        let mut f = Files::new(&c, &tmp.path().join("u.json"));
        f.scan(true);
        step(&mut f, &sent, false);
        anyhow::ensure!(sent.messages() == ["héllo wörld", "next"], "{:?}", sent.messages());
        Ok(())
    }
}
