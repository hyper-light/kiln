//! The systemd journal (`docs/observe.md` §8.1), read through `journalctl` (as Fluent Bit's and
//! Vector's journald sources read it): `--output=json --follow`, from just after the cursor of
//! the last record written (from now when there is none, or from the journal's start with `start:
//! beginning`), the source's units only when it names some. Where there is no journal (macOS,
//! Windows, a container without systemd's), its status says so.
//!
//! `KILN_JOURNALCTL` names another `journalctl` (a test's).

use std::io::{BufRead as _, BufReader, Read as _};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::time::{Duration, Instant};

use kiln_obs::logs::record::Record;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Batch, Compiled, InFlight, Offer, Start, State, Stop};

/// How long a round waits for the journal's next entry.
const POLL: Duration = Duration::from_millis(250);
/// Entries read ahead of what is offered at the most: past it, `journalctl` waits on its pipe.
const AHEAD: usize = 4_096;
/// The longest line read of an entry (`--all` prints a field whole).
const ENTRY_MAX: usize = 4 << 20;
/// How long after it exits `journalctl` is started again: doubling to the most.
const AGAIN_MIN: Duration = Duration::from_secs(1);
const AGAIN_MAX: Duration = Duration::from_secs(60);
/// How often the cursor is written down.
const SAVE_EVERY: Duration = Duration::from_secs(1);

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    cursor: Option<String>,
}

fn journalctl() -> String {
    std::env::var("KILN_JOURNALCTL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "journalctl".into())
}

/// `journalctl`'s arguments: from just after `cursor`, else from now (or the start).
fn args(c: &Compiled, cursor: Option<&str>) -> Vec<String> {
    let mut a = vec!["--output=json".to_owned(), "--follow".to_owned(), "--no-pager".to_owned(), "--all".to_owned()];
    match cursor {
        Some(cur) => a.push(format!("--after-cursor={cur}")),
        None if c.start == Start::Beginning => a.push("--no-tail".to_owned()),
        None => a.push("--lines=0".to_owned()),
    }
    for u in &c.units {
        a.push(format!("--unit={u}"));
    }
    a
}

/// A field's text: a string, or bytes as `journalctl` prints a field that is not text.
fn text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Array(b) => Some(String::from_utf8_lossy(&b.iter().filter_map(|x| x.as_u64().and_then(|x| u8::try_from(x).ok())).collect::<Vec<u8>>()).into_owned()),
        _ => None,
    }
}

/// An entry's record, and its cursor: none for a line that is not an entry.
fn entry(c: &Compiled, line: &str, now_ns: i64) -> Option<(Option<Record>, String)> {
    let Value::Object(o) = serde_json::from_str::<Value>(line).ok()? else { return None };
    let cursor = o.get("__CURSOR").and_then(Value::as_str)?.to_owned();
    let Some(mut message) = o.get("MESSAGE").and_then(text) else { return Some((None, cursor)) };
    if !c.keep(&mut message) {
        return Some((None, cursor));
    }
    let field = |k: &str| o.get(k).and_then(text).filter(|s| !s.is_empty());
    let ts = field("__REALTIME_TIMESTAMP").and_then(|t| t.parse::<i64>().ok()).and_then(|us| us.checked_mul(1_000)).filter(|&ns| ns <= now_ns + super::AHEAD_MS * 1_000_000).unwrap_or(now_ns);
    let unit = field("_SYSTEMD_UNIT");
    let service = c.service.clone().or_else(|| field("SYSLOG_IDENTIFIER")).or_else(|| unit.as_deref().map(|u| u.trim_end_matches(".service").to_owned())).or_else(|| field("_COMM")).unwrap_or_else(|| "journald".into());
    let severity = field("PRIORITY").and_then(|p| p.parse::<u8>().ok()).map_or(9, super::syslog);
    let mut attrs = Vec::new();
    for (k, name) in [("_SYSTEMD_UNIT", "systemd.unit"), ("SYSLOG_IDENTIFIER", "syslog.identifier"), ("SYSLOG_FACILITY", "syslog.facility"), ("_PID", "process.pid"), ("_COMM", "process.command"), ("_BOOT_ID", "journald.boot_id")] {
        if let Some(v) = field(k) {
            attrs.push((name.to_owned(), v));
        }
    }
    attrs.extend(c.tags.iter().cloned());
    let r = Record { ts, severity, service, host: c.host.clone(), source: c.source.clone(), message: crate::observe::cloud::message(message), attrs, ..Record::default() };
    Some((Some(r), cursor))
}

/// `journalctl` running, its lines coming through a channel (a thread reads its pipe).
struct Reading {
    child: Child,
    lines: Receiver<String>,
    errors: std::thread::JoinHandle<String>,
}

fn spawn(c: &Compiled, cursor: Option<&str>) -> std::io::Result<Reading> {
    let mut child = Command::new(journalctl()).args(args(c, cursor)).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let (tx, rx): (SyncSender<String>, Receiver<String>) = sync_channel(AHEAD);
    let out = child.stdout.take().ok_or_else(|| std::io::Error::other("no stdout"))?;
    let err = child.stderr.take().ok_or_else(|| std::io::Error::other("no stderr"))?;
    std::thread::Builder::new().name("kiln-journal-read".into()).spawn(move || {
        let mut r = BufReader::with_capacity(256 << 10, out);
        let mut line = Vec::new();
        loop {
            line.clear();
            match (&mut r).take(ENTRY_MAX as u64).read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    // Too long to be read whole: the rest of it skipped.
                    if line.last() != Some(&b'\n') {
                        let mut skip = Vec::new();
                        let _ = r.read_until(b'\n', &mut skip);
                        continue;
                    }
                    if tx.send(String::from_utf8_lossy(&line).into_owned()).is_err() {
                        return;
                    }
                }
            }
        }
    })?;
    let errors = std::thread::Builder::new().name("kiln-journal-err".into()).spawn(move || {
        let mut s = String::new();
        let _ = err.take(64 << 10).read_to_string(&mut s);
        s
    })?;
    Ok(Reading { child, lines: rx, errors })
}

/// The journal source's thread: `journalctl` read until told to stop (started again, after a
/// wait, when it ends), each record's cursor kept once its write is applied.
pub(crate) fn run(c: &Compiled, saved_to: &Path, stop: &Stop, state: &Mutex<State>, offer: Offer<'_>) {
    let mut cursor: Option<String> = std::fs::read(saved_to).ok().and_then(|b| serde_json::from_slice::<Saved>(&b).ok()).and_then(|s| s.cursor);
    let mut batch: Batch<String> = Batch::default();
    let mut flight: InFlight<String> = InFlight::default();
    let (mut records, mut excluded, mut errors, mut bytes) = (0u64, 0u64, 0u64, 0u64);
    let mut last_ms = None;
    let mut error: Option<String> = None;
    let mut saved = Instant::now();
    let mut dirty = false;
    let mut again = AGAIN_MIN;
    let save = |cursor: &Option<String>, error: &mut Option<String>| {
        if let Err(e) = serde_json::to_vec(&Saved { cursor: cursor.clone() }).map_err(std::io::Error::other).and_then(|b| crate::sync::group::save_atomic(saved_to, &b)) {
            *error = Some(format!("its cursor could not be written down: {e}"));
        }
    };
    'run: while !stop.stopped() {
        tracing::debug!(source = %c.name, cursor = cursor.as_deref().unwrap_or("(none)"), "the journal read from its cursor");
        let mut reading = match spawn(c, cursor.as_deref()) {
            Ok(r) => r,
            Err(e) => {
                errors += 1;
                error = Some(if e.kind() == std::io::ErrorKind::NotFound { format!("{} is not here: the journal is read where systemd keeps one", journalctl()) } else { format!("{}: {e}", journalctl()) });
                report(state, &error, errors, records, flight.records(), excluded, bytes, last_ms);
                stop.sleep(again);
                again = (again * 2).min(AGAIN_MAX);
                continue;
            }
        };
        let started = Instant::now();
        loop {
            if stop.stopped() {
                let _ = reading.child.kill();
                let _ = reading.child.wait();
                break 'run;
            }
            // An offer refused before goes first: nothing more is taken until it is.
            let blocked = !batch.is_empty() && !flight.offer(&mut batch, offer);
            if !blocked {
                let now_ns = crate::observe::clock_ms().saturating_mul(1_000_000);
                match reading.lines.recv_timeout(POLL) {
                    Ok(first) => {
                        let mut next = Some(first);
                        while let Some(line) = next.take() {
                            bytes += line.len() as u64;
                            if let Some((r, cur)) = entry(c, &line, now_ns) {
                                tracing::trace!(source = %c.name, cursor = %cur, record = r.is_some(), "a journal entry");
                                match r {
                                    Some(r) => batch.push(r),
                                    None => excluded += 1,
                                }
                                batch.marks.clear();
                                batch.marks.push(cur);
                            }
                            if batch.full() {
                                break;
                            }
                            next = reading.lines.try_recv().ok();
                        }
                        let _ = flight.offer(&mut batch, offer);
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => {
                        let status = reading.child.wait().ok();
                        let said = reading.errors.join().unwrap_or_default();
                        errors += 1;
                        error = Some(format!("{} ended ({}): {}", journalctl(), status.map_or_else(|| "?".into(), |s| s.to_string()), said.trim()));
                        // Started again at once if it ran a while; else after a wait, doubling.
                        if started.elapsed() > AGAIN_MAX {
                            again = AGAIN_MIN;
                        }
                        flight.written(|marks, n| {
                            if let Some(cur) = marks.into_iter().last() {
                                cursor = Some(cur);
                                dirty = true;
                            }
                            records += n;
                        });
                        report(state, &error, errors, records, flight.records(), excluded, bytes, last_ms);
                        stop.sleep(again);
                        again = (again * 2).min(AGAIN_MAX);
                        // What was read and not yet written is read again from the cursor.
                        if !flight.queue.is_empty() || !batch.is_empty() {
                            flight.drain(|marks, n| {
                                if let Some(cur) = marks.into_iter().last() {
                                    cursor = Some(cur);
                                    dirty = true;
                                }
                                records += n;
                            });
                            flight.queue.clear();
                            batch = Batch::default();
                        }
                        continue 'run;
                    }
                }
            } else {
                stop.sleep(POLL);
            }
            let before = records;
            flight.written(|marks, n| {
                if let Some(cur) = marks.into_iter().last() {
                    cursor = Some(cur);
                    dirty = true;
                }
                records += n;
            });
            if records > before {
                last_ms = Some(crate::observe::clock_ms());
                error = None;
            }
            if dirty && saved.elapsed() >= SAVE_EVERY {
                save(&cursor, &mut error);
                dirty = false;
                saved = Instant::now();
            }
            report(state, &error, errors, records, flight.records(), excluded, bytes, last_ms);
        }
    }
    flight.drain(|marks, n| {
        if let Some(cur) = marks.into_iter().last() {
            cursor = Some(cur);
        }
        records += n;
    });
    save(&cursor, &mut error);
    report(state, &error, errors, records, flight.records(), excluded, bytes, last_ms);
    tracing::info!(source = %c.name, cursor = cursor.as_deref().unwrap_or("(none)"), not_written = flight.records(), "log source stopped: its cursor written down");
}

/// The journal's last `n` entries `c` reads, nothing written (`journalctl` given 10 s).
pub(crate) fn sample(c: &Compiled, n: usize) -> super::files::Sampled {
    let mut out = super::files::Sampled { files: Vec::new(), matched: 0, records: Vec::new(), excluded: 0, error: None };
    let mut a: Vec<String> = vec!["--output=json".into(), "--no-pager".into(), "--all".into(), format!("--lines={n}")];
    a.extend(c.units.iter().map(|u| format!("--unit={u}")));
    let child = Command::new(journalctl()).args(&a).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn();
    let mut child = match child {
        Ok(ch) => ch,
        Err(e) => {
            out.error = Some(if e.kind() == std::io::ErrorKind::NotFound { format!("{} is not here: the journal is read where systemd keeps one", journalctl()) } else { format!("{}: {e}", journalctl()) });
            return out;
        }
    };
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else { return out };
    let read = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.take(16 << 20).read_to_string(&mut s);
        s
    });
    let said = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.take(64 << 10).read_to_string(&mut s);
        s
    });
    let end = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let text = read.join().unwrap_or_default();
    let now_ns = crate::observe::clock_ms().saturating_mul(1_000_000);
    for line in text.lines() {
        match entry(c, line, now_ns) {
            Some((Some(r), _)) => out.records.push(r),
            Some((None, _)) => out.excluded += 1,
            None => {}
        }
    }
    match status {
        Some(s) if !s.success() => out.error = Some(format!("{} ended ({s}): {}", journalctl(), said.join().unwrap_or_default().trim())),
        None => out.error = Some(format!("{} took more than 10 s", journalctl())),
        _ => {}
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn report(state: &Mutex<State>, error: &Option<String>, errors: u64, records: u64, in_flight: u64, excluded: u64, bytes: u64, last_ms: Option<i64>) {
    let mut s = super::lock(state);
    s.records = records;
    s.in_flight = in_flight;
    s.excluded = excluded;
    s.bytes = bytes;
    s.last_ms = last_ms;
    s.error.clone_from(error);
    s.errors = errors;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::collect::{Encoding, Format, Kind, Source};

    #[test]
    fn an_entry_reads_as_a_record_with_its_cursor() -> anyhow::Result<()> {
        let s = Source {
            kind: Kind::Journald,
            paths: Vec::new(),
            exclude: Vec::new(),
            format: Format::Auto,
            start: Start::End,
            encoding: Encoding::Utf8,
            units: vec!["nginx.service".into()],
            listen: Vec::new(),
            timezone: None,
            service: None,
            source: None,
            tags: vec!["env:prod".into()],
            rules: Vec::new(),
            on: "*".into(),
        };
        let c = s.compile("journal", "web-1").map_err(anyhow::Error::msg)?;
        anyhow::ensure!(args(&c, Some("s=1")).contains(&"--after-cursor=s=1".to_owned()) && args(&c, None).contains(&"--lines=0".to_owned()) && args(&c, None).contains(&"--unit=nginx.service".to_owned()));
        let line = r#"{"__CURSOR":"s=abc;i=1","__REALTIME_TIMESTAMP":"1792324800000001","PRIORITY":"3","_SYSTEMD_UNIT":"nginx.service","SYSLOG_IDENTIFIER":"nginx","_PID":"812","MESSAGE":[104,105,255]}"#;
        let (r, cur) = entry(&c, line, 1_800_000_000_000_000_000).ok_or_else(|| anyhow::anyhow!("an entry"))?;
        let r = r.ok_or_else(|| anyhow::anyhow!("a record"))?;
        anyhow::ensure!(cur == "s=abc;i=1" && r.ts == 1_792_324_800_000_001_000 && r.severity == 17 && r.service == "nginx" && r.source == "journald" && r.message == "hi\u{fffd}", "{r:?}");
        anyhow::ensure!(r.attrs.iter().any(|(k, v)| k == "systemd.unit" && v == "nginx.service") && r.attrs.iter().any(|(k, v)| k == "env" && v == "prod"));
        // An entry without a message still moves the cursor.
        let (r, cur) = entry(&c, r#"{"__CURSOR":"s=abc;i=2"}"#, 0).ok_or_else(|| anyhow::anyhow!("an entry"))?;
        anyhow::ensure!(r.is_none() && cur == "s=abc;i=2");
        anyhow::ensure!(entry(&c, "not json", 0).is_none());
        Ok(())
    }
}
