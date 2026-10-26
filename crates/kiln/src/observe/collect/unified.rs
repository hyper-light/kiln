//! macOS's unified log (`docs/observe.md` §8.1), read through `log`: `log stream --style ndjson
//! --type log` at the source's level (`default`, `info` or `debug`) and its predicate (an
//! NSPredicate: `subsystem == "com.example.app"`), its events as they come. After a restart, `log
//! show` from the last event written to just past the stream's start first (the stream started
//! before it, so nothing falls between), of the events the log keeps on disk (the default level
//! and above: info and debug ones are held in memory), and the stream's events the catch-up held
//! dropped. An
//! event is known by its boot (`log stream` leaves its events' empty and `log show` names it: this
//! boot's, `kern.bootsessionuuid`), its mach time (the machine's monotonic clock; both name it
//! alike) and its thread (no two of a thread's events share a mach time), and taken once: never by
//! order alone, as `log stream`
//! gives different processes' events out of their mach order (two `logger`s a millisecond apart
//! came the other way round, 2026-10-25, and the one behind was dropped as read). Its place is the
//! last event written and those written in the [`WINDOW_NS`] before it: after a restart an event
//! before the window was written, one of it was written if it is among them, and any other is
//! taken. Where there is no unified log (Linux, Windows), its status says so.
//!
//! `KILN_LOG_TOOL` names another `log` (a test's).

use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::process::Command;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use kiln_obs::logs::record::Record;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Batch, Compiled, InFlight, Offer, Reading, Start, State, Stop};

/// How long a round waits for the next event.
const POLL: Duration = Duration::from_millis(250);
/// How long after it exits `log` is started again: doubling to the most.
const AGAIN_MIN: Duration = Duration::from_secs(1);
const AGAIN_MAX: Duration = Duration::from_secs(60);
/// How often the place is written down.
const SAVE_EVERY: Duration = Duration::from_secs(1);
/// How far before the last event written the place keeps those written: far past how long `log
/// stream` holds an event behind a later one of another process (a millisecond, 2026-10-25).
const WINDOW_NS: i64 = 1_000_000_000;
/// The most events the place keeps of its window (the newest): all of a second's at 2,048 a second.
const RECENT_MAX: usize = 2_048;
/// The events a run took last that it won't take again: the catch-up and the stream both read the
/// moments between the stream's start and the catch-up's end, and those are the catch-up's last.
const READ_KEYS: usize = 16_384;

/// An event's place: its boot, its mach time, its time (ns since the epoch) and its thread.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Mark {
    pub boot: String,
    pub mach: u64,
    pub ts_ns: i64,
    /// 0 in a place written before threads were (an older kiln's).
    #[serde(default)]
    pub thread: u64,
}

/// An event's identity: its boot's, its mach time and its thread.
type Key = (u64, u64, u64);

fn key(m: &Mark) -> Key {
    // FNV-1a: a boot is one of a few, and its events are told apart by their mach times.
    let boot = m.boot.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3));
    (boot, m.mach, m.thread)
}

impl Mark {
    /// Whether `e` comes after this: of its boot, by mach time; of another, by time.
    fn before(&self, e: &Mark) -> bool {
        if e.boot == self.boot { e.mach > self.mach } else { e.ts_ns > self.ts_ns }
    }
}

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    last: Option<Mark>,
    /// The events written in the window before `last`, of its boot: each its mach time and thread.
    #[serde(default)]
    recent: Vec<(u64, u64)>,
}

/// Which events a run takes: each once. One the place says was written (before its window, or one
/// of its window's) is not taken again, nor one this run read already; any other is, whatever its
/// order.
struct Seen {
    /// The place's window's first event: one before it was written.
    floor: Option<Mark>,
    /// A place an older kiln wrote (no window): its last event and those before it were written.
    inclusive: bool,
    written: HashSet<Key>,
    read: HashSet<Key>,
    order: VecDeque<Key>,
}

impl Seen {
    fn new(saved: &Saved) -> Self {
        let Some(last) = &saved.last else {
            return Self { floor: None, inclusive: false, written: HashSet::new(), read: HashSet::new(), order: VecDeque::new() };
        };
        let of = |(mach, thread): (u64, u64)| Mark { boot: last.boot.clone(), mach, ts_ns: last.ts_ns, thread };
        let written: HashSet<Key> = saved.recent.iter().map(|&r| key(&of(r))).chain(std::iter::once(key(last))).collect();
        let first = saved.recent.iter().map(|&(mach, _)| mach).min().filter(|&m| m < last.mach);
        let floor = first.map_or_else(|| last.clone(), |mach| Mark { mach, ..last.clone() });
        Self { floor: Some(floor), inclusive: saved.recent.is_empty(), written, read: HashSet::new(), order: VecDeque::new() }
    }

    /// Whether `e` is taken (and so known from now on as read).
    fn takes(&mut self, e: &Mark) -> bool {
        let k = key(e);
        if self.read.contains(&k) || self.written.contains(&k) {
            return false;
        }
        if let Some(f) = &self.floor
            && (e.before(f) || (self.inclusive && !f.before(e)))
        {
            return false;
        }
        if self.order.len() >= READ_KEYS
            && let Some(old) = self.order.pop_front()
        {
            self.read.remove(&old);
        }
        self.read.insert(k);
        self.order.push_back(k);
        true
    }
}

/// This boot's id, as `log show` names its events' (`kern.bootsessionuuid`): `log stream` leaves
/// theirs empty (macOS 26), so an event both read is known as one. Empty where it can't be asked.
/// `sysctl` by its path: a service's PATH (launchd's, the tests' fleet's) has no `/usr/sbin`.
fn this_boot() -> &'static str {
    static BOOT: OnceLock<String> = OnceLock::new();
    BOOT.get_or_init(|| {
        let out = Command::new("/usr/sbin/sysctl").args(["-n", "kern.bootsessionuuid"]).output();
        out.ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()).unwrap_or_default()
    })
}

fn log_tool() -> String {
    std::env::var("KILN_LOG_TOOL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "log".into())
}

/// The level read: `default`, `info` (the default) or `debug`.
fn level(c: &Compiled) -> &str {
    c.level.as_deref().unwrap_or("info")
}

/// `log stream`'s arguments: from now.
fn stream_args(c: &Compiled) -> Vec<String> {
    let mut a: Vec<String> = ["stream", "--style", "ndjson", "--type", "log", "--level"].into_iter().map(str::to_owned).collect();
    a.push(level(c).to_owned());
    if let Some(p) = &c.predicate {
        a.extend(["--predicate".to_owned(), p.clone()]);
    }
    a
}

/// `log show`'s arguments: from `from` (seconds since the epoch; the start of what is kept when
/// none) to `to`. The events the log keeps on disk only (the default level, errors and faults):
/// info and debug events are held in memory, and `log show --info` reads them slowly (18 s for a
/// minute of a laptop's, against 1.5 s without), so what came at those levels while the source was
/// not reading is not read.
fn show_args(c: &Compiled, from: Option<i64>, to: i64) -> Vec<String> {
    let mut a: Vec<String> = ["show", "--style", "ndjson", "--no-signpost", "--no-loss"].into_iter().map(str::to_owned).collect();
    if let Some(f) = from {
        a.extend(["--start".to_owned(), format!("@{f}")]);
    }
    a.extend(["--end".to_owned(), format!("@{to}")]);
    if let Some(p) = &c.predicate {
        a.extend(["--predicate".to_owned(), p.clone()]);
    }
    a
}

/// `2026-10-19 04:01:27.177196-0700` as ns since the epoch.
fn time(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let n = |r: std::ops::Range<usize>| s.get(r).and_then(|x| x.parse::<i64>().ok());
    if b.get(4) != Some(&b'-') || b.get(10) != Some(&b' ') {
        return None;
    }
    let (y, mo, d, h, mi, sec) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    let mut at = 19;
    let mut frac = 0i64;
    if b.get(at) == Some(&b'.') {
        let start = at + 1;
        at = start;
        while b.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
        }
        let digits = s.get(start..at.min(start + 9))?;
        frac = digits.parse::<i64>().ok()? * 10i64.pow(9 - u32::try_from(digits.len()).ok()?);
    }
    let off = match b.get(at) {
        Some(&c) if c == b'+' || c == b'-' => {
            let v = n(at + 1..at + 3)? * 3600 + n(at + 3..at + 5)? * 60;
            if c == b'-' { -v } else { v }
        }
        _ => 0,
    };
    let secs = kiln_obs::query::calendar::days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec - off;
    secs.checked_mul(1_000_000_000)?.checked_add(frac)
}

/// A unified log message type's severity (OpenTelemetry's numbers): `Fault` a fatal one,
/// `Error`, `Default` a notice (INFO2), `Info`, `Debug`.
fn severity(t: &str) -> u8 {
    match t {
        "Fault" => 21,
        "Error" => 17,
        "Default" => 10,
        "Debug" => 5,
        _ => 9,
    }
}

/// An event's record, and its mark: none for a line that is not a log event.
pub(crate) fn event(c: &Compiled, line: &str, now_ns: i64) -> Option<(Option<Record>, Mark)> {
    let Value::Object(o) = serde_json::from_str::<Value>(line).ok()? else { return None };
    if o.get("eventType").and_then(Value::as_str) != Some("logEvent") {
        return None;
    }
    let field = |k: &str| o.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
    let ts_ns = field("timestamp").and_then(time)?;
    let thread = o.get("threadID").and_then(Value::as_u64).unwrap_or(0);
    let boot = field("bootUUID").map_or_else(|| this_boot().to_owned(), str::to_owned);
    let mark = Mark { boot, mach: o.get("machTimestamp").and_then(Value::as_u64)?, ts_ns, thread };
    let mut message = field("eventMessage").unwrap_or_default().to_owned();
    if !c.keep(&mut message) {
        return Some((None, mark));
    }
    let ts = if ts_ns <= now_ns + super::AHEAD_MS * 1_000_000 { ts_ns } else { now_ns };
    let process = field("processImagePath").map(|p| p.rsplit('/').next().unwrap_or(p).to_owned());
    let service = c.service.clone().or_else(|| process.clone()).unwrap_or_else(|| "unified_log".into());
    let mut attrs = Vec::new();
    for (k, name) in [("subsystem", "macos.subsystem"), ("category", "macos.category"), ("messageType", "macos.message_type"), ("processImagePath", "process.executable.path"), ("senderImagePath", "macos.sender_image_path")] {
        if let Some(v) = field(k) {
            attrs.push((name.to_owned(), v.to_owned()));
        }
    }
    for (k, name) in [("processID", "process.pid"), ("threadID", "thread.id")] {
        if let Some(v) = o.get(k).and_then(Value::as_u64) {
            attrs.push((name.to_owned(), v.to_string()));
        }
    }
    if let Some(p) = process {
        attrs.push(("process.command".to_owned(), p));
    }
    attrs.extend(c.tags.iter().cloned());
    let severity = field("messageType").map_or(9, severity);
    let r = Record { ts, severity, service, host: c.host.clone(), source: c.source.clone(), message: crate::observe::cloud::message(message), attrs, ..Record::default() };
    Some((Some(r), mark))
}

fn now_s() -> i64 {
    crate::observe::clock_ms().div_euclid(1000)
}

fn spawn(args: &[String]) -> std::io::Result<Reading> {
    super::read_lines(Command::new(log_tool()).args(args), "unified-log")
}

fn missing(e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::NotFound { format!("{} is not here: the unified log is macOS's", log_tool()) } else { format!("{}: {e}", log_tool()) }
}

/// What a run of the source counts, and where it is.
struct Run<'a> {
    c: &'a Compiled,
    state: &'a Mutex<State>,
    batch: Batch<Mark>,
    flight: InFlight<Mark>,
    seen: Seen,
    /// The last event written, and those written in the window before it (oldest first).
    written: Option<Mark>,
    recent: VecDeque<Mark>,
    records: u64,
    excluded: u64,
    errors: u64,
    bytes: u64,
    last_ms: Option<i64>,
    error: Option<String>,
    dirty: bool,
}

/// Marks of events written: the last of them (by its place) and the window before it.
fn note_written(written: &mut Option<Mark>, recent: &mut VecDeque<Mark>, marks: Vec<Mark>) {
    for m in marks {
        if written.as_ref().is_none_or(|w| w.before(&m)) {
            *written = Some(m.clone());
        }
        recent.push_back(m);
    }
    let Some(last) = written.as_ref() else { return };
    while recent.front().is_some_and(|f| f.ts_ns < last.ts_ns - WINDOW_NS) || recent.len() > RECENT_MAX {
        recent.pop_front();
    }
}

impl<'a> Run<'a> {
    fn new(c: &'a Compiled, state: &'a Mutex<State>, place: &Saved) -> Self {
        let recent = match &place.last {
            Some(last) => place.recent.iter().map(|&(mach, thread)| Mark { boot: last.boot.clone(), mach, ts_ns: last.ts_ns, thread }).collect(),
            None => VecDeque::new(),
        };
        Run {
            c,
            state,
            batch: Batch::default(),
            flight: InFlight::default(),
            seen: Seen::new(place),
            written: place.last.clone(),
            recent,
            records: 0,
            excluded: 0,
            errors: 0,
            bytes: 0,
            last_ms: None,
            error: None,
            dirty: false,
        }
    }
}

impl Run<'_> {
    /// One line read: an event not taken before goes into the batch.
    fn line(&mut self, line: &str, now_ns: i64) {
        self.bytes += line.len() as u64;
        let Some((r, mark)) = event(self.c, line, now_ns) else { return };
        if !self.seen.takes(&mark) {
            return;
        }
        match r {
            Some(r) => self.batch.push(r),
            None => self.excluded += 1,
        }
        self.batch.marks.push(mark);
    }

    fn written_marks(&mut self) {
        let mut records = 0;
        let (flight, written, recent, dirty) = (&mut self.flight, &mut self.written, &mut self.recent, &mut self.dirty);
        flight.written(|marks, n| {
            note_written(written, recent, marks);
            *dirty = true;
            records += n;
        });
        if records > 0 {
            self.records += records;
            self.last_ms = Some(crate::observe::clock_ms());
            self.error = None;
        }
    }

    /// Where it is, as written down: the last event written and its window's, of its boot.
    fn saved(&self) -> Saved {
        let recent = match &self.written {
            Some(last) => self.recent.iter().filter(|m| m.boot == last.boot).map(|m| (m.mach, m.thread)).collect(),
            None => Vec::new(),
        };
        Saved { last: self.written.clone(), recent }
    }

    /// Where a catch-up reads from: the second before a window before its window's first event (a
    /// place read back knows its window's events by their mach times, their own times not kept).
    fn catch_up_from(&self) -> Option<i64> {
        let first = self.recent.iter().chain(self.written.iter()).map(|m| m.ts_ns).min()?;
        Some((first - WINDOW_NS).div_euclid(1_000_000_000) - 1)
    }

    fn report(&self) {
        let mut s = super::lock(self.state);
        s.records = self.records;
        s.in_flight = self.flight.records();
        s.excluded = self.excluded;
        s.bytes = self.bytes;
        s.last_ms = self.last_ms;
        s.error.clone_from(&self.error);
        s.errors = self.errors;
    }

    /// Reads `reading` until it ends (`Ok`) or the source is told to stop (`Err`), offering as it
    /// goes and taking in what is written; its place saved each second.
    fn pump(&mut self, reading: &mut Reading, stop: &Stop, offer: Offer<'_>, save: &dyn Fn(&Saved) -> Result<(), String>, saved: &mut Instant) -> Result<(), ()> {
        let mut quiet = None;
        loop {
            if stop.stopped() {
                reading.stop();
                return Err(());
            }
            // An offer refused before goes first: nothing more is taken until it is.
            let blocked = !self.batch.is_empty() && !self.flight.offer(&mut self.batch, offer);
            if blocked {
                stop.sleep(POLL);
            } else {
                let now_ns = crate::observe::clock_ms().saturating_mul(1_000_000);
                match reading.lines.recv_timeout(POLL) {
                    Ok(first) => {
                        let mut next = Some(first);
                        while let Some(l) = next.take() {
                            self.line(&l, now_ns);
                            if self.batch.full() {
                                break;
                            }
                            next = reading.lines.try_recv().ok();
                        }
                        let _ = self.flight.offer(&mut self.batch, offer);
                    }
                    Err(RecvTimeoutError::Timeout) if !super::ended(reading, &mut quiet) => {}
                    Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                        // What was read is offered before the next process reads on.
                        while !self.batch.is_empty() && !self.flight.offer(&mut self.batch, offer) {
                            if stop.stopped() {
                                return Err(());
                            }
                            stop.sleep(POLL);
                        }
                        return Ok(());
                    }
                }
            }
            self.written_marks();
            if self.dirty && saved.elapsed() >= SAVE_EVERY {
                if let Err(e) = save(&self.saved()) {
                    self.error = Some(e);
                }
                self.dirty = false;
                *saved = Instant::now();
            }
            self.report();
        }
    }
}

/// The unified log source's thread: `log stream` read until told to stop (started again, after a
/// wait, when it ends), after `log show` of what came since the last event written; each event's
/// mark kept once its write is applied.
pub(crate) fn run(c: &Compiled, saved_to: &Path, stop: &Stop, state: &Mutex<State>, offer: Offer<'_>) {
    let place: Saved = std::fs::read(saved_to).ok().and_then(|b| serde_json::from_slice::<Saved>(&b).ok()).unwrap_or_default();
    let save = |s: &Saved| -> Result<(), String> {
        serde_json::to_vec(s).map_err(std::io::Error::other).and_then(|b| crate::sync::group::save_atomic(saved_to, &b)).map_err(|e| format!("its place could not be written down: {e}"))
    };
    let mut run = Run::new(c, state, &place);
    let mut saved = Instant::now();
    let mut again = AGAIN_MIN;
    let mut first = true;
    'run: while !stop.stopped() {
        // The stream first, so nothing falls between the catch-up's end and its start.
        let mut stream = match spawn(&stream_args(c)) {
            Ok(r) => r,
            Err(e) => {
                run.errors += 1;
                run.error = Some(missing(&e));
                run.report();
                stop.sleep(again);
                again = (again * 2).min(AGAIN_MAX);
                continue;
            }
        };
        let started = Instant::now();
        // What came since the last event written (or, the source asking, everything kept): read
        // whole first; the stream's events it held are dropped as the stream is read.
        let from = run.catch_up_from();
        if from.is_some() || (first && c.start == Start::Beginning) {
            tracing::debug!(source = %c.name, from = ?from, "the unified log read since its last event");
            match spawn(&show_args(c, from, now_s() + 1)) {
                Ok(mut show) => {
                    if run.pump(&mut show, stop, offer, &save, &mut saved).is_err() {
                        stream.stop();
                        break 'run;
                    }
                    let _ = show.child.wait();
                }
                Err(e) => {
                    run.errors += 1;
                    run.error = Some(missing(&e));
                }
            }
        }
        first = false;
        if run.pump(&mut stream, stop, offer, &save, &mut saved).is_err() {
            break 'run;
        }
        let status = stream.child.wait().ok();
        let said = stream.errors.join().unwrap_or_default();
        run.errors += 1;
        run.error = Some(format!("{} ended ({}): {}", log_tool(), status.map_or_else(|| "?".into(), |s| s.to_string()), said.trim()));
        if started.elapsed() > AGAIN_MAX {
            again = AGAIN_MIN;
        }
        run.report();
        stop.sleep(again);
        again = (again * 2).min(AGAIN_MAX);
    }
    let (flight, written, recent, records) = (&mut run.flight, &mut run.written, &mut run.recent, &mut run.records);
    flight.drain(|marks, n| {
        note_written(written, recent, marks);
        *records += n;
    });
    if let Err(e) = save(&run.saved()) {
        run.error = Some(e);
    }
    run.report();
    tracing::info!(source = %c.name, last = ?run.written, not_written = run.flight.records(), "log source stopped: its place written down");
}

/// The unified log's last minute of events `c` reads, `n` of them, nothing written (`log`
/// given 10 s).
pub(crate) fn sample(c: &Compiled, n: usize) -> super::files::Sampled {
    let mut out = super::files::Sampled { files: Vec::new(), matched: 0, records: Vec::new(), excluded: 0, error: None };
    let to = now_s();
    let (lines, status, said) = match super::output_lines(Command::new(log_tool()).args(show_args(c, Some(to - 60), to)), "unified-log", Duration::from_secs(10)) {
        Ok(r) => r,
        Err(e) => {
            out.error = Some(missing(&e));
            return out;
        }
    };
    let now_ns = crate::observe::clock_ms().saturating_mul(1_000_000);
    let mut kept: std::collections::VecDeque<Record> = std::collections::VecDeque::new();
    for line in &lines {
        match event(c, line, now_ns) {
            Some((Some(r), _)) => {
                kept.push_back(r);
                if kept.len() > n {
                    kept.pop_front();
                }
            }
            Some((None, _)) => out.excluded += 1,
            None => {}
        }
    }
    match status {
        None => out.error = Some(format!("{} took more than 10 s", log_tool())),
        Some(s) if !s.success() => out.error = Some(format!("{} ended ({s}): {}", log_tool(), said.trim())),
        _ => {}
    }
    out.records = kept.into();
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::observe::collect::{Kind, Source};

    fn source() -> Source {
        serde_json::from_value(serde_json::json!({ "type": "unified_log", "predicate": "subsystem == \"com.example.app\"", "level": "debug", "tags": ["team:mac"] })).unwrap()
    }

    /// A line as `log stream --style ndjson` writes it (macOS 26).
    const LINE: &str = r#"{"timezoneName":"","messageType":"Error","eventType":"logEvent","source":null,"formatString":"%s","userID":501,"activityIdentifier":0,"subsystem":"com.example.app","category":"net","threadID":287582287,"senderImageUUID":"59E25FE6-64ED-3963-A307-F7C4D40A6A3E","backtrace":{"frames":[{"imageOffset":1764,"imageUUID":"59E25FE6-64ED-3963-A307-F7C4D40A6A3E"}]},"bootUUID":"E571B146-3D40-47EB-A907-AF472B516F9D","processImagePath":"\/Applications\/Example.app\/Contents\/MacOS\/Example","senderImagePath":"\/usr\/lib\/libnetwork.dylib","timestamp":"2026-10-19 04:01:27.177196-0700","machTimestamp":25584498816458,"eventMessage":"connection refused","processImageUUID":"59E25FE6-64ED-3963-A307-F7C4D40A6A3E","traceID":13353070231556,"processID":67508,"senderProgramCounter":1764,"parentActivityIdentifier":0}"#;

    #[test]
    fn an_event_reads_as_a_record_with_its_mark() {
        let c = source().compile("mac", "mbp").unwrap();
        assert_eq!(c.kind, Kind::UnifiedLog);
        let (r, mark) = event(&c, LINE, i64::MAX / 2).unwrap();
        let r = r.unwrap();
        // 04:01:27.177196 at -07:00 is 11:01:27.177196 UTC.
        assert_eq!(r.ts, 1_792_407_687_177_196_000);
        assert_eq!(mark, Mark { boot: "E571B146-3D40-47EB-A907-AF472B516F9D".into(), mach: 25_584_498_816_458, ts_ns: r.ts, thread: 287_582_287 });
        assert_eq!((r.severity, r.service.as_str(), r.source.as_str(), r.host.as_str(), r.message.as_str()), (17, "Example", "unified_log", "mbp", "connection refused"));
        let attr = |k: &str| r.attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str());
        assert_eq!((attr("macos.subsystem"), attr("macos.category"), attr("process.pid"), attr("team")), (Some("com.example.app"), Some("net"), Some("67508"), Some("mac")));
        // `log stream`'s events name no boot: this boot's, as `log show` names theirs.
        let (_, streamed) = event(&c, &LINE.replace("E571B146-3D40-47EB-A907-AF472B516F9D", ""), i64::MAX / 2).unwrap();
        assert_eq!(streamed.boot, this_boot());
        // Not a log event, and not JSON: nothing (the stream's first line says its filter).
        assert!(event(&c, &LINE.replace("\"logEvent\"", "\"activityCreateEvent\""), 0).is_none());
        assert!(event(&c, "Filtering the log data using \"subsystem == \\\"x\\\"\"", 0).is_none());
    }

    /// On a Mac, this boot's id is the one `log show` gives its events (a UUID).
    #[cfg(target_os = "macos")]
    #[test]
    fn this_boot_is_named_as_log_show_names_it() {
        let b = this_boot();
        assert!(b.len() == 36 && b.bytes().filter(|&c| c == b'-').count() == 4, "{b:?}");
    }

    #[test]
    fn a_mark_orders_events_by_boot_and_mach_time() {
        let m = Mark { boot: "a".into(), mach: 10, ts_ns: 1_000, thread: 1 };
        assert!(m.before(&Mark { boot: "a".into(), mach: 11, ts_ns: 900, thread: 1 }), "of a boot, mach time decides");
        assert!(!m.before(&Mark { boot: "a".into(), mach: 10, ts_ns: 2_000, thread: 1 }));
        assert!(m.before(&Mark { boot: "b".into(), mach: 1, ts_ns: 1_001, thread: 1 }), "another boot's, later");
        assert!(!m.before(&Mark { boot: "z".into(), mach: 99, ts_ns: 999, thread: 1 }), "another boot's, earlier: the one before");
    }

    /// `LINE` as event `n` of mach time `mach` on thread `thread`.
    fn line(mach: u64, thread: u64, n: u64) -> String {
        LINE.replace("25584498816458", &mach.to_string()).replace("287582287", &thread.to_string()).replace("connection refused", &format!("n={n}"))
    }

    fn taken(run: &mut Run<'_>) -> Vec<String> {
        run.batch.take_records().into_iter().map(|r| r.message).collect()
    }

    /// `log stream` gives different processes' events out of their mach order: each is taken, once,
    /// whatever came before it (2026-10-25: the one behind a millisecond's swap was dropped as read).
    #[test]
    fn events_out_of_their_mach_order_are_each_taken_once() {
        let c = source().compile("mac", "mbp").unwrap();
        let state = Mutex::new(State::default());
        let mut run = Run::new(&c, &state, &Saved::default());
        for (mach, thread, n) in [(100, 1, 0), (300, 2, 2), (200, 3, 1), (300, 2, 2), (400, 1, 3), (200, 3, 1)] {
            run.line(&line(mach, thread, n), i64::MAX / 2);
        }
        assert_eq!(taken(&mut run), ["n=0", "n=2", "n=1", "n=3"]);
        assert_eq!(run.batch.marks.iter().map(|m| m.mach).collect::<Vec<_>>(), [100, 300, 200, 400], "each event's mark");
    }

    /// After a restart, what the place says was written is not taken again (an event before its
    /// window, or one of its window's), and the rest is: one in flight when it stopped (in its
    /// window, not written), the new ones, and the stream's after the catch-up's, once each.
    #[test]
    fn a_restart_takes_what_its_place_says_was_not_written() {
        let c = source().compile("mac", "mbp").unwrap();
        let state = Mutex::new(State::default());
        let boot = "E571B146-3D40-47EB-A907-AF472B516F9D";
        let place = Saved { last: Some(Mark { boot: boot.into(), mach: 500, ts_ns: 5_000_000_000, thread: 1 }), recent: vec![(300, 2), (400, 1), (500, 1)] };
        let mut run = Run::new(&c, &state, &place);
        assert_eq!(run.catch_up_from(), Some(3), "a window before the window, and a second more");
        // The catch-up (`log show`), then the stream (its first events the catch-up's last).
        for (mach, thread, n) in [(100, 1, 1), (300, 2, 3), (350, 3, 35), (400, 1, 4), (500, 1, 5), (600, 1, 6)] {
            run.line(&line(mach, thread, n), i64::MAX / 2);
        }
        for (mach, thread, n) in [(600, 1, 6), (700, 1, 7), (650, 4, 65)] {
            run.line(&line(mach, thread, n), i64::MAX / 2);
        }
        assert_eq!(taken(&mut run), ["n=35", "n=6", "n=7", "n=65"]);
        // A place an older kiln wrote (no window, no threads): its last event and those before it.
        let old: Saved = serde_json::from_str(&format!(r#"{{"last":{{"boot":"{boot}","mach":500,"ts_ns":5000000000}}}}"#)).unwrap();
        let mut run = Run::new(&c, &state, &old);
        for (mach, thread, n) in [(450, 2, 45), (500, 9, 5), (501, 9, 51)] {
            run.line(&line(mach, thread, n), i64::MAX / 2);
        }
        assert_eq!(taken(&mut run), ["n=51"]);
    }

    /// The place: the last event written by its place (not the last to be written), and those written
    /// in the window before it, the oldest let go; written down and read back as the same place.
    #[test]
    fn the_place_is_the_last_event_written_and_its_window() {
        let c = source().compile("mac", "mbp").unwrap();
        let state = Mutex::new(State::default());
        let mark = |mach: u64, thread: u64, ts_ns: i64| Mark { boot: "b".into(), mach, ts_ns, thread };
        let mut run = Run::new(&c, &state, &Saved::default());
        note_written(&mut run.written, &mut run.recent, vec![mark(100, 1, 1_000_000_000), mark(300, 2, 2_500_000_000), mark(200, 3, 2_400_000_000)]);
        assert_eq!(run.written, Some(mark(300, 2, 2_500_000_000)), "the latest by mach time, not the last written");
        assert_eq!(run.recent.iter().map(|m| m.mach).collect::<Vec<_>>(), [300, 200], "100 is more than a second before");
        let saved = run.saved();
        assert_eq!(saved.recent, [(300, 2), (200, 3)]);
        let back: Saved = serde_json::from_slice(&serde_json::to_vec(&saved).unwrap()).unwrap();
        assert_eq!(back, saved);
        // A window of more than RECENT_MAX events keeps the newest.
        note_written(&mut run.written, &mut run.recent, (0..RECENT_MAX as u64 + 10).map(|i| mark(1_000 + i, 1, 2_600_000_000)).collect());
        assert_eq!(run.recent.len(), RECENT_MAX);
        assert_eq!(run.recent.front().map(|m| m.mach), Some(1_010));
    }

    #[test]
    fn the_arguments_ask_for_what_the_source_says() {
        let c = source().compile("mac", "mbp").unwrap();
        assert_eq!(stream_args(&c), ["stream", "--style", "ndjson", "--type", "log", "--level", "debug", "--predicate", "subsystem == \"com.example.app\""]);
        assert_eq!(show_args(&c, Some(100), 200), ["show", "--style", "ndjson", "--no-signpost", "--no-loss", "--start", "@100", "--end", "@200", "--predicate", "subsystem == \"com.example.app\""]);
        let plain: Source = serde_json::from_value(serde_json::json!({ "type": "unified_log" })).unwrap();
        let p = plain.compile("m", "h").unwrap();
        assert_eq!(show_args(&p, None, 5), ["show", "--style", "ndjson", "--no-signpost", "--no-loss", "--end", "@5"]);
        for bad in [serde_json::json!({ "type": "unified_log", "level": "loud" }), serde_json::json!({ "type": "unified_log", "paths": ["/var/log/x"] }), serde_json::json!({ "type": "file", "paths": ["/x"], "predicate": "a == 1" })] {
            let s: Source = serde_json::from_value(bad.clone()).unwrap();
            assert!(s.check().is_err(), "{bad}");
        }
    }
}
