//! macOS's unified log (`docs/observe.md` §8.1), read through `log`: `log stream --style ndjson
//! --type log` at the source's level (`default`, `info` or `debug`) and its predicate (an
//! NSPredicate: `subsystem == "com.example.app"`), its events as they come. After a restart, `log
//! show` from the last event written to just past the stream's start first (the stream started
//! before it, so nothing falls between), of the events the log keeps on disk (the default level
//! and above: info and debug ones are held in memory), and the stream's events the catch-up held
//! dropped. An
//! event is known by its boot and its mach time (the machine's monotonic clock, which no two of a
//! boot's events share on one thread): one later than the last written is taken, of another boot
//! by its time. Where there is no unified log (Linux, Windows), its status says so.
//!
//! `KILN_LOG_TOOL` names another `log` (a test's).

use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::sync::mpsc::RecvTimeoutError;
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

/// The last event written: its boot, its mach time and its time (ns since the epoch).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Mark {
    pub boot: String,
    pub mach: u64,
    pub ts_ns: i64,
}

impl Mark {
    /// Whether `e` comes after this: of its boot, by mach time; of another, by time.
    fn before(&self, e: &Mark) -> bool {
        if e.boot == self.boot { e.mach > self.mach } else { e.ts_ns > self.ts_ns }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    last: Option<Mark>,
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
    let mark = Mark { boot: field("bootUUID").unwrap_or_default().to_owned(), mach: o.get("machTimestamp").and_then(Value::as_u64)?, ts_ns };
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
    /// The last event taken (written or on its way), and the last written.
    taken: Option<Mark>,
    written: Option<Mark>,
    records: u64,
    excluded: u64,
    errors: u64,
    bytes: u64,
    last_ms: Option<i64>,
    error: Option<String>,
    dirty: bool,
}

impl Run<'_> {
    /// One line read: an event later than any taken goes into the batch.
    fn line(&mut self, line: &str, now_ns: i64) {
        self.bytes += line.len() as u64;
        let Some((r, mark)) = event(self.c, line, now_ns) else { return };
        if self.taken.as_ref().is_some_and(|t| !t.before(&mark)) {
            return;
        }
        match r {
            Some(r) => self.batch.push(r),
            None => self.excluded += 1,
        }
        self.batch.marks.clear();
        self.batch.marks.push(mark.clone());
        self.taken = Some(mark);
    }

    fn written_marks(&mut self) {
        let (mut records, mut last) = (0, None);
        self.flight.written(|marks, n| {
            if let Some(m) = marks.into_iter().last() {
                last = Some(m);
            }
            records += n;
        });
        if let Some(m) = last {
            self.written = Some(m);
            self.dirty = true;
        }
        if records > 0 {
            self.records += records;
            self.last_ms = Some(crate::observe::clock_ms());
            self.error = None;
        }
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
    fn pump(&mut self, reading: &mut Reading, stop: &Stop, offer: Offer<'_>, save: &dyn Fn(&Option<Mark>) -> Result<(), String>, saved: &mut Instant) -> Result<(), ()> {
        let mut quiet = None;
        loop {
            if stop.stopped() {
                let _ = reading.child.kill();
                let _ = reading.child.wait();
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
                if let Err(e) = save(&self.written) {
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
    let mark: Option<Mark> = std::fs::read(saved_to).ok().and_then(|b| serde_json::from_slice::<Saved>(&b).ok()).and_then(|s| s.last);
    let save = |m: &Option<Mark>| -> Result<(), String> {
        serde_json::to_vec(&Saved { last: m.clone() }).map_err(std::io::Error::other).and_then(|b| crate::sync::group::save_atomic(saved_to, &b)).map_err(|e| format!("its place could not be written down: {e}"))
    };
    let mut run = Run { c, state, batch: Batch::default(), flight: InFlight::default(), taken: mark.clone(), written: mark, records: 0, excluded: 0, errors: 0, bytes: 0, last_ms: None, error: None, dirty: false };
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
        let from = run.written.as_ref().map(|m| m.ts_ns.div_euclid(1_000_000_000) - 1);
        if from.is_some() || (first && c.start == Start::Beginning) {
            tracing::debug!(source = %c.name, from = ?from, "the unified log read since its last event");
            match spawn(&show_args(c, from, now_s() + 1)) {
                Ok(mut show) => {
                    if run.pump(&mut show, stop, offer, &save, &mut saved).is_err() {
                        let _ = stream.child.kill();
                        let _ = stream.child.wait();
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
    run.flight.drain(|marks, n| {
        if let Some(m) = marks.into_iter().last() {
            run.written = Some(m);
        }
        run.records += n;
    });
    if let Err(e) = save(&run.written) {
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
        assert_eq!(mark, Mark { boot: "E571B146-3D40-47EB-A907-AF472B516F9D".into(), mach: 25_584_498_816_458, ts_ns: r.ts });
        assert_eq!((r.severity, r.service.as_str(), r.source.as_str(), r.host.as_str(), r.message.as_str()), (17, "Example", "unified_log", "mbp", "connection refused"));
        let attr = |k: &str| r.attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str());
        assert_eq!((attr("macos.subsystem"), attr("macos.category"), attr("process.pid"), attr("team")), (Some("com.example.app"), Some("net"), Some("67508"), Some("mac")));
        // Not a log event, and not JSON: nothing (the stream's first line says its filter).
        assert!(event(&c, &LINE.replace("\"logEvent\"", "\"activityCreateEvent\""), 0).is_none());
        assert!(event(&c, "Filtering the log data using \"subsystem == \\\"x\\\"\"", 0).is_none());
    }

    #[test]
    fn a_mark_orders_events_by_boot_and_mach_time() {
        let m = Mark { boot: "a".into(), mach: 10, ts_ns: 1_000 };
        assert!(m.before(&Mark { boot: "a".into(), mach: 11, ts_ns: 900 }), "of a boot, mach time decides");
        assert!(!m.before(&Mark { boot: "a".into(), mach: 10, ts_ns: 2_000 }));
        assert!(m.before(&Mark { boot: "b".into(), mach: 1, ts_ns: 1_001 }), "another boot's, later");
        assert!(!m.before(&Mark { boot: "z".into(), mach: 99, ts_ns: 999 }), "another boot's, earlier: the one before");
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
