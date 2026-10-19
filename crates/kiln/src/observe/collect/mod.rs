//! Logs collected on every machine (`docs/observe.md` §8.1): files tailed as Datadog's Agent and
//! Vector tail them, Kubernetes' and Docker's container logs among them, and the systemd journal,
//! by the server or a runner.
//!
//! - A source is its type, what it reads (a file source's paths, globs, less those it leaves; the
//!   journal's units), how its lines read (a format, an encoding), what its records are called
//!   (`service`, `source`, tags as attributes), Datadog's processing rules (`exclude_at_match`,
//!   `include_at_match`, `mask_sequences`, `multi_line`), and the machine that reads it: the
//!   server, a runner by its name, or every machine (`*`), each its own.
//! - A record's place in what it came from (a file's offset, the journal's cursor) is kept only
//!   once the write holding it is applied ([`super::ingest::Ack`]), so a machine stopped or killed
//!   reads on from there: a crash repeats what was in flight (seconds of it) and loses nothing
//!   read; a stop repeats nothing.
//! - The server keeps the sources (`<observe>/log_sources.json`) and tells each runner those it
//!   reads, as it tells scrape targets; each machine says how its sources go in its status.

pub mod files;
pub mod journald;
pub mod syslog;

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use kiln_obs::logs::record::Record;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ingest::Ack;
pub use super::scrape::{EVERY, SERVER};

/// Sources kept at the most.
pub const SOURCES: usize = 256;
/// The longest record a line makes: a longer line is cut into records this long, each said to be
/// cut (Datadog's Agent's 256 kB).
pub const MAX_LINE: usize = 256 << 10;
/// What one offer of records holds at the most: records and bytes.
const BATCH_RECORDS: usize = 1_000;
const BATCH_BYTES: usize = 1 << 20;
/// How long a stopping source waits for its last writes to be applied before it writes its
/// places down.
const STOP_WAIT: Duration = Duration::from_secs(2);
/// A record's time past this far ahead of the machine's clock is taken as now: a time without a
/// zone read as UTC on a machine west of it would otherwise be refused as too far ahead.
const AHEAD_MS: i64 = 60_000;
/// Attributes a record takes from a JSON line at the most, and the longest value.
const ATTRS_MAX: usize = 128;
const VALUE_MAX: usize = 16 << 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Files by their paths' globs.
    File,
    /// Every container's log on a Kubernetes node (`/var/log/pods`), its pod and container named.
    Kubernetes,
    /// Every container's log Docker's `json-file` driver keeps (`/var/lib/docker/containers`).
    Docker,
    /// The systemd journal, read through `journalctl`.
    Journald,
    /// Syslog received over UDP and TCP (RFC 5424, RFC 3164).
    Syslog,
}

/// How a file's lines read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    /// JSON where a line is a JSON object, text otherwise.
    #[default]
    Auto,
    Text,
    Json,
    /// The container runtime interface's: `<time> <stream> <P|F> <text>`, a long line in parts.
    Cri,
    /// Docker's `json-file`: `{"log": ..., "stream": ..., "time": ...}`, a long line in parts.
    Docker,
}

/// Where a file found when its source starts, with no place kept, is read from. A file that
/// appears later is read from its beginning.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Start {
    #[default]
    End,
    Beginning,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Encoding {
    #[default]
    #[serde(rename = "utf-8")]
    Utf8,
    #[serde(rename = "utf-16le")]
    Utf16Le,
    #[serde(rename = "utf-16be")]
    Utf16Be,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleKind {
    /// A record whose text matches goes.
    ExcludeAtMatch,
    /// A record whose text does not match goes.
    IncludeAtMatch,
    /// What matches is replaced by `replace_placeholder` (a token, a card's number).
    MaskSequences,
    /// A record starts at a line that matches; the lines after it, to the next, are its own.
    MultiLine,
}

/// Datadog's Agent's `log_processing_rules`, in the order given (a `multi_line` rule first,
/// whatever its place).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    #[serde(rename = "type")]
    pub kind: RuleKind,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    pub pattern: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace_placeholder: Option<String>,
}

fn on_default() -> String {
    SERVER.to_owned()
}

/// A source: what to read, how, what its records are, who reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    #[serde(rename = "type")]
    pub kind: Kind,
    /// A file source's globs (`/var/log/nginx/*.log`, `/srv/**/app.log`); Kubernetes' and
    /// Docker's their own unless given.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// Globs of paths not read (`*.gz`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub format: Format,
    #[serde(default)]
    pub start: Start,
    #[serde(default)]
    pub encoding: Encoding,
    /// The journal's units read (`nginx.service`); every unit when none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub units: Vec<String>,
    /// Where a syslog source listens: `udp://0.0.0.0:514`, `tcp://0.0.0.0:6514`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub listen: Vec<String>,
    /// The zone an RFC 3164 time (which has none) is in: an IANA name; UTC unless said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// `key:value`, an attribute of every record.
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub rules: Vec<Rule>,
    /// `server`, a runner's name, or `*` (every machine, each its own).
    #[serde(default = "on_default")]
    pub on: String,
}

/// A path is absolute: `/…` on Unix, `C:\…` or `\\server\…` on Windows.
fn absolute(p: &str) -> bool {
    let b = p.as_bytes();
    p.starts_with('/') || p.starts_with("\\\\") || (b.len() > 2 && b.first().is_some_and(u8::is_ascii_alphabetic) && b.get(1) == Some(&b':') && matches!(b.get(2), Some(b'\\' | b'/')))
}

fn regex(pattern: &str) -> Result<regex::Regex, String> {
    if pattern.is_empty() || pattern.len() > 1024 {
        return Err("a rule's pattern: 1 to 1024 characters".into());
    }
    regex::RegexBuilder::new(pattern).size_limit(1 << 20).build().map_err(|e| format!("a rule's pattern does not compile: {e}"))
}

impl Source {
    pub fn check(&self) -> Result<(), String> {
        match self.kind {
            Kind::File if self.paths.is_empty() => return Err("a file source's paths: a glob at the least (/var/log/app/*.log)".into()),
            Kind::Journald | Kind::Syslog if !self.paths.is_empty() || !self.exclude.is_empty() => return Err("paths are a file source's".into()),
            Kind::Journald | Kind::Syslog if self.format != Format::Auto || self.encoding != Encoding::Utf8 => return Err("the journal's entries and syslog's messages are records already: no format or encoding".into()),
            Kind::File | Kind::Kubernetes | Kind::Docker | Kind::Syslog if !self.units.is_empty() => return Err("units are the journal's".into()),
            Kind::Syslog if self.listen.is_empty() || self.listen.len() > 4 => return Err("a syslog source listens somewhere: udp://0.0.0.0:514, tcp://0.0.0.0:6514 (4 at the most)".into()),
            Kind::File | Kind::Kubernetes | Kind::Docker | Kind::Journald if !self.listen.is_empty() || self.timezone.is_some() => return Err("listen and timezone are a syslog source's".into()),
            _ => {}
        }
        for l in &self.listen {
            syslog::listen(l)?;
        }
        if let Some(z) = &self.timezone {
            kiln_obs::query::calendar::Zone::named(z)?;
        }
        if self.paths.len() > 64 || self.exclude.len() > 64 {
            return Err("64 paths at the most, and 64 left out".into());
        }
        for p in self.paths.iter().chain(&self.exclude) {
            if !absolute(p) {
                return Err(format!("a path is absolute: {p}"));
            }
            glob::Pattern::new(p).map_err(|e| format!("a path's glob: {p}: {e}"))?;
        }
        if self.units.len() > 64 || self.units.iter().any(|u| u.is_empty() || u.len() > 256 || u.chars().any(char::is_control)) {
            return Err("units: 64 at the most, each a unit's name".into());
        }
        for (what, v) in [("service", &self.service), ("source", &self.source)] {
            if v.as_deref().is_some_and(|s| s.trim().is_empty() || s.len() > 100) {
                return Err(format!("a {what}: 1 to 100 characters"));
            }
        }
        for t in &self.tags {
            if t.split_once(':').is_none_or(|(k, v)| k.is_empty() || v.is_empty()) {
                return Err(format!("a tag is key:value: {t}"));
            }
        }
        if self.rules.len() > 32 {
            return Err("32 rules at the most".into());
        }
        let mut multi = 0;
        for r in &self.rules {
            regex(&r.pattern)?;
            match r.kind {
                RuleKind::MaskSequences if r.replace_placeholder.as_deref().is_none_or(|p| p.len() > 256) => return Err("a mask_sequences rule's replace_placeholder: up to 256 characters".into()),
                RuleKind::MultiLine if matches!(self.kind, Kind::Journald | Kind::Syslog) => return Err("the journal's entries and syslog's messages are records already: no multi_line rule".into()),
                RuleKind::MultiLine => multi += 1,
                _ => {}
            }
        }
        if multi > 1 {
            return Err("one multi_line rule at the most".into());
        }
        if self.on.is_empty() || self.on.len() > 255 {
            return Err("on: `server`, a runner's name, or `*`".into());
        }
        Ok(())
    }

    /// Whether `machine` reads it (`server` for the server).
    pub fn read_by(&self, machine: &str) -> bool {
        self.on == EVERY || self.on == machine
    }

    /// The globs it reads: its own, or its type's.
    pub fn globs(&self) -> Vec<String> {
        if !self.paths.is_empty() {
            return self.paths.clone();
        }
        match self.kind {
            Kind::Kubernetes => vec!["/var/log/pods/*/*/*.log".into()],
            Kind::Docker => vec!["/var/lib/docker/containers/*/*-json.log".into()],
            Kind::File | Kind::Journald | Kind::Syslog => Vec::new(),
        }
    }

    /// What it is compiled to on `host`: `name`'s, checked.
    pub(crate) fn compile(&self, name: &str, host: &str) -> Result<Compiled, String> {
        self.check()?;
        let format = match self.kind {
            Kind::Kubernetes if self.format == Format::Auto => Format::Cri,
            Kind::Docker if self.format == Format::Auto => Format::Docker,
            _ => self.format,
        };
        let mut steps = Vec::new();
        let mut multiline = None;
        for r in &self.rules {
            let re = regex(&r.pattern)?;
            match r.kind {
                RuleKind::ExcludeAtMatch => steps.push(Step::Exclude(re)),
                RuleKind::IncludeAtMatch => steps.push(Step::Include(re)),
                RuleKind::MaskSequences => steps.push(Step::Mask(re, r.replace_placeholder.clone().unwrap_or_default())),
                RuleKind::MultiLine => multiline = Some(re),
            }
        }
        let default_source = match self.kind {
            Kind::File => "file",
            Kind::Kubernetes => "kubernetes",
            Kind::Docker => "docker",
            Kind::Journald => "journald",
            Kind::Syslog => "syslog",
        };
        Ok(Compiled {
            name: name.to_owned(),
            kind: self.kind,
            globs: self.globs(),
            exclude: self.exclude.iter().filter_map(|p| glob::Pattern::new(p).ok()).collect(),
            format,
            start: self.start,
            encoding: self.encoding,
            units: self.units.clone(),
            listen: self.listen.clone(),
            timezone: self.timezone.as_deref().map_or(Ok(kiln_obs::query::calendar::Zone::UTC), kiln_obs::query::calendar::Zone::named)?,
            service: self.service.clone(),
            source: self.source.clone().unwrap_or_else(|| default_source.to_owned()),
            tags: self.tags.iter().filter_map(|t| t.split_once(':')).map(|(k, v)| (k.to_owned(), v.to_owned())).collect(),
            steps,
            multiline,
            host: host.to_owned(),
        })
    }
}

/// A processing rule, compiled.
pub(crate) enum Step {
    Exclude(regex::Regex),
    Include(regex::Regex),
    Mask(regex::Regex, String),
}

/// A source as a machine reads it.
pub(crate) struct Compiled {
    pub name: String,
    pub kind: Kind,
    pub globs: Vec<String>,
    pub exclude: Vec<glob::Pattern>,
    pub format: Format,
    pub start: Start,
    pub encoding: Encoding,
    pub units: Vec<String>,
    pub listen: Vec<String>,
    pub timezone: kiln_obs::query::calendar::Zone,
    pub service: Option<String>,
    pub source: String,
    pub tags: Vec<(String, String)>,
    pub steps: Vec<Step>,
    pub multiline: Option<regex::Regex>,
    pub host: String,
}

/// A line as its format gives it: its text, and what the format says of it.
#[derive(Debug, Default)]
pub(crate) struct Line {
    pub text: String,
    /// Nanoseconds since the epoch.
    pub ts: Option<i64>,
    pub stream: Option<&'static str>,
    pub truncated: bool,
}

/// What a file's records say of where they came from: its path, and its container's or pod's
/// names.
#[derive(Debug, Default, Clone)]
pub(crate) struct Origin {
    pub attrs: Vec<(String, String)>,
    pub service: Option<String>,
}

impl Compiled {
    /// The rules applied to a record's text: `false` when it goes.
    pub(crate) fn keep(&self, text: &mut String) -> bool {
        for s in &self.steps {
            match s {
                Step::Exclude(r) if r.is_match(text) => return false,
                Step::Include(r) if !r.is_match(text) => return false,
                Step::Mask(r, with) => {
                    if let Cow::Owned(masked) = r.replace_all(text, regex::NoExpand(with)) {
                        *text = masked;
                    }
                }
                _ => {}
            }
        }
        true
    }

    /// A line's record: a JSON line's message, level, time, trace and fields, or a text line's
    /// text and the level it starts with.
    pub(crate) fn record(&self, line: Line, origin: &Origin, now_ns: i64) -> Record {
        let parse = match self.format {
            Format::Json => true,
            Format::Auto | Format::Cri | Format::Docker => line.text.trim_start().starts_with('{'),
            Format::Text => false,
        };
        let object = if parse { serde_json::from_str::<Value>(&line.text).ok().and_then(|v| if let Value::Object(o) = v { Some(o) } else { None }) } else { None };
        // Its attributes sized once: the line's fields, where it came from, the source's tags.
        let fields = object.as_ref().map_or(0, serde_json::Map::len);
        let mut r = Record {
            ts: line.ts.unwrap_or(now_ns),
            service: self.service.clone().or_else(|| origin.service.clone()).unwrap_or_else(|| self.name.clone()),
            host: self.host.clone(),
            source: self.source.clone(),
            attrs: Vec::with_capacity(fields + origin.attrs.len() + self.tags.len() + 2),
            ..Record::default()
        };
        match object {
            Some(mut o) => {
                let message = ["message", "msg", "log", "MESSAGE", "text", "@m", "@mt"].iter().find_map(|k| if let Some(Value::String(_)) = o.get(*k) { o.remove(*k) } else { None });
                r.message = match message {
                    Some(Value::String(m)) => m,
                    _ => line.text.clone(),
                };
                // The first of each that is there: taken out when it reads as one, else left as a field.
                let first = |o: &serde_json::Map<String, Value>, keys: &[&'static str]| keys.iter().copied().find(|k| o.contains_key(*k));
                r.severity = match first(&o, &["level", "severity", "levelname", "log.level", "status", "lvl", "loglevel", "@l"]) {
                    Some(k) if let Some(l) = o.get(k).and_then(level) => {
                        o.remove(k);
                        l
                    }
                    _ => super::cloud::severity_of(&r.message),
                };
                if let Some(k) = first(&o, &["timestamp", "time", "@timestamp", "ts", "date", "_timestamp", "Timestamp", "eventTime", "@t"])
                    && let Some(ts) = o.get(k).and_then(time_ns)
                {
                    o.remove(k);
                    r.ts = ts;
                }
                if self.service.is_none()
                    && let Some(Value::String(s)) = o.get("service").or_else(|| o.get("service.name"))
                    && !s.is_empty()
                {
                    r.service = s.clone();
                }
                r.trace_id = ["trace_id", "traceId", "trace.id"].iter().find_map(|k| o.get(*k).and_then(Value::as_str).and_then(hex_id::<16>));
                r.span_id = ["span_id", "spanId", "span.id"].iter().find_map(|k| o.get(*k).and_then(Value::as_str).and_then(hex_id::<8>));
                for (k, v) in o {
                    if ["trace_id", "traceId", "trace.id", "span_id", "spanId", "span.id", "service", "service.name"].contains(&k.as_str()) {
                        continue;
                    }
                    flatten(k, v, 1, &mut r.attrs);
                }
            }
            None => {
                r.severity = super::cloud::severity_of(&line.text);
                r.message = line.text;
            }
        }
        r.message = super::cloud::message(std::mem::take(&mut r.message));
        if r.ts > now_ns + AHEAD_MS * 1_000_000 {
            r.ts = now_ns;
        }
        r.attrs.extend(origin.attrs.iter().cloned());
        if let Some(s) = line.stream {
            r.attrs.push(("log.iostream".into(), s.into()));
        }
        if line.truncated {
            r.attrs.push(("truncated".into(), "true".into()));
        }
        r.attrs.extend(self.tags.iter().cloned());
        r
    }
}

/// A level as JSON logs give it: a name, or a number (bunyan's and pino's 10–60, syslog's 0–7).
fn level(v: &Value) -> Option<u8> {
    match v {
        Value::String(s) => kiln_obs::logs::record::severities(s.trim()).map(|r| *r.start()),
        Value::Number(n) => Some(match n.as_u64()? {
            10 => 1,
            20 => 5,
            30 => 9,
            40 => 13,
            50 => 17,
            60 => 21,
            p @ 0..=7 => syslog(p as u8),
            _ => return None,
        }),
        _ => None,
    }
}

/// A syslog priority's severity (OpenTelemetry's mapping: emergency to critical fatal, error,
/// warning, notice and informational both info, debug).
pub(crate) fn syslog(p: u8) -> u8 {
    match p {
        0..=2 => 21,
        3 => 17,
        4 => 13,
        5 => 10,
        6 => 9,
        _ => 5,
    }
}

/// A time as JSON logs give it: RFC 3339, or a number of seconds, milliseconds, microseconds or
/// nanoseconds since the epoch (told apart by their size).
fn time_ns(v: &Value) -> Option<i64> {
    match v {
        Value::String(s) => super::cloud::parse_time_ns(s),
        Value::Number(n) => {
            // Whole numbers exactly; fractions (seconds with their decimals) as floats.
            if let Some(i) = n.as_i64().filter(|&i| i > 0) {
                return match i {
                    ..100_000_000_000 => i.checked_mul(1_000_000_000),
                    100_000_000_000..100_000_000_000_000 => i.checked_mul(1_000_000),
                    100_000_000_000_000..100_000_000_000_000_000 => i.checked_mul(1_000),
                    _ => Some(i),
                };
            }
            let f = n.as_f64()?;
            let ns = if f < 1e11 {
                f * 1e9
            } else if f < 1e14 {
                f * 1e6
            } else if f < 1e17 {
                f * 1e3
            } else {
                f
            };
            (ns.is_finite() && ns > 0.0 && ns < 9.2e18).then_some(ns as i64)
        }
        _ => None,
    }
}

fn hex_id<const N: usize>(s: &str) -> Option<[u8; N]> {
    let mut out = [0u8; N];
    if s.len() != N * 2 {
        return None;
    }
    hex::decode_to_slice(s, &mut out).ok()?;
    out.iter().any(|&b| b != 0).then_some(out)
}

/// A value cut to the longest an attribute holds, at a character.
fn cut(mut s: String) -> String {
    if s.len() > VALUE_MAX {
        let mut c = VALUE_MAX;
        while !s.is_char_boundary(c) {
            c -= 1;
        }
        s.truncate(c);
    }
    s
}

/// A JSON line's field as attributes, taken (its key and text moved, not copied): nested
/// objects' keys joined by dots, arrays as JSON.
fn flatten(key: String, v: Value, depth: usize, out: &mut Vec<(String, String)>) {
    if out.len() >= ATTRS_MAX {
        return;
    }
    match v {
        Value::Object(o) if depth < 4 => {
            for (k, v) in o {
                flatten(format!("{key}.{k}"), v, depth + 1, out);
            }
        }
        Value::Null => {}
        Value::String(s) => out.push((key, cut(s))),
        Value::Bool(_) | Value::Number(_) => out.push((key, v.to_string())),
        Value::Array(_) | Value::Object(_) => out.push((key, cut(v.to_string()))),
    }
}

/// How a machine's source goes, as its status says: its counts since `since_ms`, when this
/// machine's process began reading it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// Unix ms.
    pub since_ms: Option<i64>,
    /// Files open, and files matched but not read (past the bound of open files).
    pub files: u64,
    pub over_limit: u64,
    /// Bytes read, records written (their writes applied) and in flight, records the rules took
    /// out, lines cut at [`MAX_LINE`].
    pub bytes: u64,
    pub records: u64,
    pub in_flight: u64,
    pub excluded: u64,
    pub truncated: u64,
    /// Bytes of the open files not yet written.
    pub lag_bytes: u64,
    /// A syslog source's TCP connections open, and datagrams dropped for want of room.
    pub connections: u64,
    pub dropped: u64,
    /// Unix ms of the last record written.
    pub last_ms: Option<i64>,
    pub error: Option<String>,
    pub errors: u64,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A source's thread told to stop, woken from its sleep.
#[derive(Default)]
pub(crate) struct Stop {
    stopped: AtomicBool,
    thread: Mutex<Option<std::thread::Thread>>,
}

impl Stop {
    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(t) = lock(&self.thread).as_ref() {
            t.unpark();
        }
    }

    pub(crate) fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    /// Sleeps for `d`, or until told to stop.
    pub(crate) fn sleep(&self, d: Duration) {
        let end = Instant::now() + d;
        while !self.stopped() {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            std::thread::park_timeout(left);
        }
    }
}

/// Where records go: offered with an ack; refused (no room), they come back, to be offered again.
pub(crate) type Offer<'a> = &'a (dyn Fn(Vec<Record>, Ack) -> Result<(), Vec<Record>> + Sync);

/// The observe owner's offer: what a machine's sources write through.
fn offer_to_observe(records: Vec<Record>, ack: Ack) -> Result<(), Vec<Record>> {
    match super::api::get() {
        Some(o) => o.offer_logs_acked(records, ack),
        None => Err(records),
    }
}

/// What a source would read on this machine now, nothing written: the files it matches and the
/// records of the newest one's last lines (the journal's last entries), as the API shows them.
pub fn test(name: &str, s: &Source, host: &str) -> Value {
    let c = match s.compile(name, host) {
        Ok(c) => c,
        Err(e) => return serde_json::json!({ "ok": false, "error": e }),
    };
    let t0 = Instant::now();
    let sampled = match c.kind {
        Kind::Journald => journald::sample(&c, 20),
        Kind::File | Kind::Kubernetes | Kind::Docker => files::sample(&c, 20),
        // Whether it may listen where it says, here: each address bound a moment.
        Kind::Syslog => files::Sampled {
            files: Vec::new(),
            matched: 0,
            records: Vec::new(),
            excluded: 0,
            error: {
                let failed: Vec<String> = c
                    .listen
                    .iter()
                    .filter_map(|l| {
                        let (udp, addr) = syslog::listen(l).ok()?;
                        let bound = if udp { std::net::UdpSocket::bind(addr).map(drop) } else { std::net::TcpListener::bind(addr).map(drop) };
                        bound.err().map(|e| format!("{l}: {e}"))
                    })
                    .collect();
                (!failed.is_empty()).then(|| failed.join("; "))
            },
        },
    };
    let records: Vec<Value> = sampled
        .records
        .iter()
        .map(|r| {
            serde_json::json!({
                "ts_ns": r.ts,
                "status": kiln_obs::logs::record::level(r.severity),
                "service": r.service,
                "host": r.host,
                "source": r.source,
                "message": r.message.chars().take(2_000).collect::<String>(),
                "attributes": r.attrs.iter().map(|(k, v)| (k.clone(), Value::from(v.clone()))).collect::<serde_json::Map<_, _>>(),
            })
        })
        .collect();
    serde_json::json!({
        "ok": sampled.error.is_none() && (matches!(c.kind, Kind::Journald | Kind::Syslog) || sampled.matched > 0),
        "files": sampled.files.iter().map(|(p, b)| serde_json::json!({ "path": p, "bytes": b })).collect::<Vec<_>>(),
        "matched": sampled.matched,
        "records": records,
        "excluded": sampled.excluded,
        "error": sampled.error.or_else(|| (!matches!(c.kind, Kind::Journald | Kind::Syslog) && sampled.matched == 0).then(|| "no file here matches its paths".to_owned())),
        "took_ms": u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX),
    })
}

/// For benches (`benches/collect.rs`): every file `source` matches, read whole on this thread as a
/// machine reads it, its places kept in `places`, each offer written at once: the records and the
/// bytes read.
#[doc(hidden)]
pub fn read_whole(name: &str, source: &Source, places: &Path) -> Result<(u64, u64), String> {
    let c = source.compile(name, "bench")?;
    Ok(files::read_whole(&c, places))
}

// --------------------------------------------------------------------------------------- store

/// The server's sources by name, kept in `<observe>/log_sources.json`.
pub struct Sources {
    path: PathBuf,
    all: Mutex<BTreeMap<String, Source>>,
}

impl Sources {
    pub fn open(dir: &Path) -> Self {
        let path = dir.join("log_sources.json");
        let all = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Self { path, all: Mutex::new(all) }
    }

    pub fn list(&self) -> Vec<(String, Source)> {
        lock(&self.all).iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    pub fn get(&self, name: &str) -> Option<Source> {
        lock(&self.all).get(name).cloned()
    }

    pub fn put(&self, name: &str, s: Source) -> Result<(), String> {
        if name.is_empty() || name.len() > 64 || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)) || name.starts_with('.') {
            return Err("a source's name is 1 to 64 letters, digits, '-', '_' and '.', not starting with '.'".into());
        }
        s.check()?;
        let mut all = lock(&self.all);
        if !all.contains_key(name) && all.len() >= SOURCES {
            return Err(format!("{SOURCES} sources at the most"));
        }
        all.insert(name.to_owned(), s);
        let bytes = serde_json::to_vec_pretty(&*all).map_err(|e| e.to_string())?;
        drop(all);
        crate::sync::group::save_atomic(&self.path, &bytes).map_err(|e| e.to_string())
    }

    pub fn delete(&self, name: &str) -> anyhow::Result<bool> {
        let mut all = lock(&self.all);
        let had = all.remove(name).is_some();
        let bytes = serde_json::to_vec_pretty(&*all)?;
        drop(all);
        crate::sync::group::save_atomic(&self.path, &bytes)?;
        Ok(had)
    }
}

// ------------------------------------------------------------------------------------ collector

/// A machine's sources: each it reads on a thread of its own.
pub struct Collector {
    machine: String,
    host: String,
    /// Where each source's places are kept: `<observe>/collect/<name>.json`.
    dir: PathBuf,
    running: Mutex<HashMap<String, Running>>,
}

struct Running {
    source: Source,
    stop: Arc<Stop>,
    state: Arc<Mutex<State>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

static COLLECTOR: OnceLock<Arc<Collector>> = OnceLock::new();

/// This process's collector, started once: `machine` the name sources give it (`server` for the
/// server), `host` its records' host, `dir` observe's folder on this machine. Its sources are told
/// to stop when `stop` is cancelled: each waits a moment for its last writes (which come back over
/// the process's connection: keep it up a moment, [`Collector::stopped`]) and writes its places
/// down.
pub fn start(machine: &str, host: &str, dir: &Path, stop: crate::rt::Shutdown) -> &'static Arc<Collector> {
    COLLECTOR.get_or_init(|| {
        let c = Arc::new(Collector { machine: machine.to_owned(), host: host.to_owned(), dir: dir.join("collect"), running: Mutex::new(HashMap::new()) });
        let watch = c.clone();
        crate::rt::spawn(async move {
            stop.cancelled().await;
            watch.stop_signal();
        });
        c
    })
}

pub fn get() -> Option<&'static Arc<Collector>> {
    COLLECTOR.get()
}

/// A source's places' file.
fn places_file(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.json"))
}

impl Collector {
    /// Reads `sources` (of every machine: those this one reads are kept) from now: one unchanged
    /// goes on as it was, a changed one starts again from its places (once the thread before it
    /// has written them down), one gone stops (its places kept, should it come back). Never waits:
    /// it is called from where a wait would hold up others.
    pub fn set(&self, sources: &[(String, Source)]) {
        let mine: HashMap<&str, &Source> = sources.iter().filter(|(_, s)| s.read_by(&self.machine)).map(|(n, s)| (n.as_str(), s)).collect();
        let mut before: HashMap<String, std::thread::JoinHandle<()>> = HashMap::new();
        let mut running = lock(&self.running);
        running.retain(|name, r| {
            let keep = mine.get(name.as_str()).is_some_and(|s| **s == r.source);
            if !keep {
                r.stop.stop();
                if let Some(t) = r.thread.take() {
                    before.insert(name.clone(), t);
                }
            }
            keep
        });
        for (name, s) in mine {
            if running.contains_key(name) {
                continue;
            }
            let state = Arc::new(Mutex::new(State { since_ms: Some(super::clock_ms()), ..State::default() }));
            let stop = Arc::new(Stop::default());
            let prev = before.remove(name);
            let thread = match s.compile(name, &self.host) {
                Ok(c) => {
                    let (stop2, state2, file) = (stop.clone(), state.clone(), places_file(&self.dir, name));
                    std::thread::Builder::new()
                        .name(format!("kiln-logs-{name}"))
                        .spawn(move || {
                            *lock(&stop2.thread) = Some(std::thread::current());
                            if let Some(p) = prev {
                                let _ = p.join();
                            }
                            match c.kind {
                                Kind::Journald => journald::run(&c, &file, &stop2, &state2, &offer_to_observe),
                                Kind::File | Kind::Kubernetes | Kind::Docker => files::run(&c, &file, &stop2, &state2, &offer_to_observe),
                                Kind::Syslog => syslog::run(c, &stop2, &state2, &(Arc::new(offer_to_observe) as syslog::Sink)),
                            }
                        })
                        .map_err(|e| lock(&state).error = Some(format!("its thread did not start: {e}")))
                        .ok()
                }
                Err(e) => {
                    lock(&state).error = Some(e);
                    None
                }
            };
            running.insert(name.to_owned(), Running { source: s.clone(), stop, state, thread });
        }
    }

    /// How each of this machine's sources goes.
    pub fn states(&self) -> Vec<(String, State)> {
        let mut out: Vec<(String, State)> = lock(&self.running).iter().map(|(n, r)| (n.clone(), lock(&r.state).clone())).collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Tells every source to stop: each waits a moment for its last writes, then writes its places
    /// down.
    pub fn stop_signal(&self) {
        for r in lock(&self.running).values() {
            r.stop.stop();
        }
    }

    /// Whether every source's thread has ended.
    pub fn stopped(&self) -> bool {
        lock(&self.running).values().all(|r| r.thread.as_ref().is_none_or(std::thread::JoinHandle::is_finished))
    }

    /// Stops every source and waits, `within` at the most, for them to write their places down.
    pub fn stop_within(&self, within: Duration) {
        self.stop_signal();
        let end = Instant::now() + within;
        while !self.stopped() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Records gathered for one offer, and where each source they came from may move its place once
/// they are written.
pub(crate) struct Batch<M> {
    pub records: Vec<Record>,
    pub bytes: usize,
    pub marks: Vec<M>,
}

impl<M> Default for Batch<M> {
    fn default() -> Self {
        Self { records: Vec::new(), bytes: 0, marks: Vec::new() }
    }
}

impl<M> Batch<M> {
    pub(crate) fn push(&mut self, r: Record) {
        self.bytes += super::ingest::record_bytes(&r);
        self.records.push(r);
    }

    pub(crate) fn full(&self) -> bool {
        self.records.len() >= BATCH_RECORDS || self.bytes >= BATCH_BYTES
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.records.is_empty() && self.marks.is_empty()
    }
}

/// Offers sent and not yet written: each its ack, its marks and its records' count.
pub(crate) struct InFlight<M> {
    pub queue: std::collections::VecDeque<(Ack, Vec<M>, u64)>,
}

impl<M> Default for InFlight<M> {
    fn default() -> Self {
        Self { queue: std::collections::VecDeque::new() }
    }
}

impl<M> InFlight<M> {
    /// Offers `b` (its records and marks taken when it is taken): `false` when refused, `b` kept.
    pub(crate) fn offer(&mut self, b: &mut Batch<M>, offer: Offer<'_>) -> bool {
        if b.is_empty() {
            return true;
        }
        let ack = Ack::new();
        let n = b.records.len() as u64;
        match offer(std::mem::take(&mut b.records), ack.clone()) {
            Ok(()) => {
                self.queue.push_back((ack, std::mem::take(&mut b.marks), n));
                b.bytes = 0;
                true
            }
            Err(back) => {
                b.records = back;
                false
            }
        }
    }

    /// The marks of the offers written, oldest first, with their records' count.
    pub(crate) fn written(&mut self, mut each: impl FnMut(Vec<M>, u64)) {
        while self.queue.front().is_some_and(|(a, _, _)| a.done()) {
            if let Some((_, marks, n)) = self.queue.pop_front() {
                each(marks, n);
            }
        }
    }

    pub(crate) fn records(&self) -> u64 {
        self.queue.iter().map(|q| q.2).sum()
    }

    /// Waits up to [`STOP_WAIT`] for every offer to be written.
    pub(crate) fn drain(&mut self, mut each: impl FnMut(Vec<M>, u64)) {
        let end = Instant::now() + STOP_WAIT;
        loop {
            self.written(&mut each);
            if self.queue.is_empty() || Instant::now() >= end {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_source() -> Source {
        Source {
            kind: Kind::File,
            paths: vec!["/var/log/app/*.log".into()],
            exclude: Vec::new(),
            format: Format::Auto,
            start: Start::End,
            encoding: Encoding::Utf8,
            units: Vec::new(),
            listen: Vec::new(),
            timezone: None,
            service: Some("app".into()),
            source: None,
            tags: vec!["env:prod".into()],
            rules: Vec::new(),
            on: "*".into(),
        }
    }

    #[test]
    fn a_source_is_checked_as_it_is_kept() {
        assert!(file_source().check().is_ok());
        let rule = |kind, pattern: &str| Rule { kind, name: String::new(), pattern: pattern.into(), replace_placeholder: None };
        for bad in [
            Source { paths: Vec::new(), ..file_source() },
            Source { paths: vec!["relative/*.log".into()], ..file_source() },
            Source { paths: vec!["/var/log/[.log".into()], ..file_source() },
            Source { kind: Kind::Journald, ..file_source() },
            Source { units: vec!["nginx.service".into()], ..file_source() },
            Source { tags: vec!["env".into()], ..file_source() },
            Source { rules: vec![rule(RuleKind::ExcludeAtMatch, "(")], ..file_source() },
            Source { rules: vec![rule(RuleKind::MaskSequences, "\\d{16}")], ..file_source() },
            Source { rules: vec![rule(RuleKind::MultiLine, "^\\d"), rule(RuleKind::MultiLine, "^x")], ..file_source() },
            Source { kind: Kind::Journald, paths: Vec::new(), rules: vec![rule(RuleKind::MultiLine, "^\\d")], ..file_source() },
            Source { service: Some(" ".into()), ..file_source() },
            Source { on: String::new(), ..file_source() },
        ] {
            assert!(bad.check().is_err(), "{bad:?}");
        }
        assert!(Source { paths: vec!["C:\\logs\\*.log".into()], ..file_source() }.check().is_ok());
        assert!(Source { kind: Kind::Journald, paths: Vec::new(), units: vec!["nginx.service".into()], ..file_source() }.check().is_ok());
        assert!(Source { kind: Kind::Kubernetes, paths: Vec::new(), ..file_source() }.check().is_ok());
        let syslog = || Source { kind: Kind::Syslog, paths: Vec::new(), listen: vec!["udp://0.0.0.0:5514".into(), "tcp://127.0.0.1:5514".into()], timezone: Some("Europe/Paris".into()), ..file_source() };
        assert!(syslog().check().is_ok());
        for bad in [
            Source { listen: Vec::new(), ..syslog() },
            Source { listen: vec!["0.0.0.0:514".into()], ..syslog() },
            Source { timezone: Some("Mars/Olympus".into()), ..syslog() },
            Source { paths: vec!["/var/log/*.log".into()], ..syslog() },
            Source { listen: vec!["udp://0.0.0.0:514".into()], timezone: None, ..file_source() },
        ] {
            assert!(bad.check().is_err(), "{bad:?}");
        }
        assert_eq!(Source { kind: Kind::Kubernetes, paths: Vec::new(), ..file_source() }.globs(), vec!["/var/log/pods/*/*/*.log".to_owned()]);
        let json = r#"{"type":"journald","units":["sshd.service"],"on":"*"}"#;
        let s: Source = serde_json::from_str(json).unwrap_or_else(|_| file_source());
        assert_eq!((s.kind, s.format, s.start, s.encoding), (Kind::Journald, Format::Auto, Start::End, Encoding::Utf8));
    }

    #[test]
    fn rules_exclude_include_and_mask_in_their_order() -> anyhow::Result<()> {
        let rule = |kind, pattern: &str, with: Option<&str>| Rule { kind, name: String::new(), pattern: pattern.into(), replace_placeholder: with.map(str::to_owned) };
        let s = Source {
            rules: vec![
                rule(RuleKind::ExcludeAtMatch, "healthz", None),
                rule(RuleKind::MaskSequences, "token=[A-Za-z0-9]+", Some("token=[masked]")),
                rule(RuleKind::IncludeAtMatch, "GET|POST", None),
                rule(RuleKind::MaskSequences, "\\$\\d+", Some("$1")),
            ],
            ..file_source()
        };
        let c = s.compile("app", "h").map_err(anyhow::Error::msg)?;
        let mut t = "GET /healthz 200".to_owned();
        anyhow::ensure!(!c.keep(&mut t));
        let mut t = "GET /a?token=abc123 costs $40".to_owned();
        anyhow::ensure!(c.keep(&mut t) && t == "GET /a?token=[masked] costs $1", "{t}");
        let mut t = "DELETE /a".to_owned();
        anyhow::ensure!(!c.keep(&mut t));
        Ok(())
    }

    #[test]
    fn a_json_line_gives_its_message_level_time_trace_and_fields() -> anyhow::Result<()> {
        let c = file_source().compile("app", "web-1").map_err(anyhow::Error::msg)?;
        let origin = Origin { attrs: vec![("log.file.path".into(), "/var/log/app/a.log".into())], service: None };
        let line = r#"{"level":"warn","time":"2026-10-18T12:00:00.5Z","msg":"slow","route":"/a","took":{"ms":812},"trace_id":"0af7651916cd43dd8448eb211c80319c","span_id":"b7ad6b7169203331","tags":["a","b"]}"#;
        let r = c.record(Line { text: line.into(), ..Line::default() }, &origin, 1_800_000_000_000_000_000);
        anyhow::ensure!(r.message == "slow" && r.severity == 13 && r.service == "app" && r.host == "web-1" && r.source == "file", "{r:?}");
        anyhow::ensure!(r.ts == 1_792_324_800_500_000_000, "{}", r.ts);
        anyhow::ensure!(r.trace_id.is_some() && r.span_id.is_some());
        let attr = |k: &str| r.attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str());
        anyhow::ensure!(attr("route") == Some("/a") && attr("took.ms") == Some("812") && attr("tags") == Some(r#"["a","b"]"#), "{:?}", r.attrs);
        anyhow::ensure!(attr("log.file.path").is_some() && attr("env") == Some("prod") && attr("level").is_none() && attr("trace_id").is_none());
        // Text: its level read from its words; a time ahead of the clock taken as now.
        let r = c.record(Line { text: "2026-10-18 12:00:00 ERROR payment failed".into(), ts: Some(i64::MAX / 2), ..Line::default() }, &origin, 5);
        anyhow::ensure!(r.severity == 17 && r.message.starts_with("2026") && r.ts == 5, "{r:?}");
        // Numbers: pino's levels, epoch milliseconds.
        let r = c.record(Line { text: r#"{"level":50,"time":1792324800500,"msg":"x"}"#.into(), ..Line::default() }, &origin, 1_800_000_000_000_000_000);
        anyhow::ensure!(r.severity == 17 && r.ts == 1_792_324_800_500_000_000, "{r:?}");
        Ok(())
    }
}
