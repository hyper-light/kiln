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
pub mod unified;
pub mod winevent;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use kiln_obs::logs::record::Record;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ingest::{Ack, LogPart, Logs};
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
    /// macOS's unified log, read through `log stream`.
    UnifiedLog,
    /// The Windows Event Log's channels, read through `wevtutil`.
    WindowsEvent,
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
    /// The unified log's events read: an NSPredicate (`subsystem == "com.example.app"`); every one
    /// when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predicate: Option<String>,
    /// The unified log's level read: `default`, `info` (unless said) or `debug`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    /// The Windows Event Log's channels read (`System`, `Application`,
    /// `Microsoft-Windows-Sysmon/Operational`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channels: Vec<String>,
    /// Which of their events: an XPath query, `*[...]` (`*[System[(Level=1 or Level=2)]]`); every
    /// one when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
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
            Kind::File | Kind::Kubernetes | Kind::Docker | Kind::Journald | Kind::UnifiedLog if !self.listen.is_empty() || self.timezone.is_some() => return Err("listen and timezone are a syslog source's".into()),
            Kind::UnifiedLog if !self.paths.is_empty() || !self.exclude.is_empty() || !self.units.is_empty() => return Err("paths are a file source's, units the journal's".into()),
            Kind::UnifiedLog if self.format != Format::Auto || self.encoding != Encoding::Utf8 => return Err("the unified log's events are records already: no format or encoding".into()),
            Kind::File | Kind::Kubernetes | Kind::Docker | Kind::Journald | Kind::Syslog | Kind::WindowsEvent if self.predicate.is_some() || self.level.is_some() => return Err("predicate and level are a unified log source's".into()),
            Kind::WindowsEvent if self.channels.is_empty() || self.channels.len() > 32 => return Err("a Windows event source's channels: 1 to 32 (System, Application)".into()),
            Kind::WindowsEvent if !self.paths.is_empty() || !self.exclude.is_empty() || !self.units.is_empty() || !self.listen.is_empty() || self.timezone.is_some() => return Err("paths are a file source's, units the journal's, listen and timezone syslog's".into()),
            Kind::WindowsEvent if self.format != Format::Auto || self.encoding != Encoding::Utf8 => return Err("the Event Log's events are records already: no format or encoding".into()),
            Kind::File | Kind::Kubernetes | Kind::Docker | Kind::Journald | Kind::Syslog | Kind::UnifiedLog if !self.channels.is_empty() || self.query.is_some() => return Err("channels and query are a Windows event source's".into()),
            _ => {}
        }
        if self.channels.iter().any(|ch| ch.trim().is_empty() || ch.len() > 256 || ch.chars().any(|c| c.is_control() || c == '"')) {
            return Err("a channel: its name (System, Microsoft-Windows-Sysmon/Operational)".into());
        }
        if let Some(q) = &self.query {
            let q = q.trim();
            if !(q.starts_with("*[") && q.ends_with(']')) || q.len() > 4096 || q.chars().any(char::is_control) {
                return Err("a query: an XPath of the channel's events, *[...] (*[System[(Level=1 or Level=2)]])".into());
            }
        }
        if self.predicate.as_deref().is_some_and(|p| p.trim().is_empty() || p.len() > 4096 || p.chars().any(char::is_control)) {
            return Err("a predicate: an NSPredicate of 1 to 4096 characters".into());
        }
        if self.level.as_deref().is_some_and(|l| !matches!(l, "default" | "info" | "debug")) {
            return Err("a level: default, info or debug".into());
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
                RuleKind::MultiLine if matches!(self.kind, Kind::Journald | Kind::Syslog | Kind::UnifiedLog | Kind::WindowsEvent) => return Err("the journal's entries, syslog's messages and the unified log's and the Event Log's events are records already: no multi_line rule".into()),
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
            Kind::File | Kind::Journald | Kind::Syslog | Kind::UnifiedLog | Kind::WindowsEvent => Vec::new(),
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
            Kind::UnifiedLog => "unified_log",
            Kind::WindowsEvent => "windows_event",
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
            predicate: self.predicate.clone(),
            level: self.level.clone(),
            channels: self.channels.clone(),
            query: self.query.clone(),
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
    pub predicate: Option<String>,
    pub level: Option<String>,
    pub channels: Vec<String>,
    pub query: Option<String>,
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

/// What a reader asks of each line before it makes a record of it: the pipelines active (taken
/// again each round), and their filters' scratch.
#[derive(Default)]
pub(crate) struct PipelineFilters {
    pub pipelines: Arc<Vec<super::pipeline::Pipeline>>,
    pub scratch: super::pipeline::Scratch,
}

/// A line's fields as its record would hold them ([`Compiled::record`]), borrowed (a JSON line's
/// from the room its reading keeps): what a pipeline's filter is asked of, and what its row is
/// written of, with no record made.
struct LineView<'a> {
    message: &'a str,
    service: &'a str,
    host: &'a str,
    source: &'a str,
    severity: u8,
    /// A JSON line's fields: its record's first attributes.
    fields: Option<&'a Flat>,
    origin: &'a [(String, String)],
    stream: Option<&'a str>,
    truncated: bool,
    tags: &'a [(String, String)],
    trace_id: Option<[u8; 16]>,
    span_id: Option<[u8; 8]>,
}

impl<'a> LineView<'a> {
    /// Its attributes, in its record's order.
    fn attrs(&self) -> impl Iterator<Item = (&'a str, &'a str)> + 'a {
        let pair = |(k, v): &'a (String, String)| (k.as_str(), v.as_str());
        self.fields
            .into_iter()
            .flat_map(Flat::iter)
            .chain(self.origin.iter().map(pair))
            .chain(self.stream.map(|s| ("log.iostream", s)))
            .chain(self.truncated.then_some(("truncated", "true")))
            .chain(self.tags.iter().map(pair))
    }
}

impl kiln_obs::logs::search::Fields for LineView<'_> {
    fn message(&self) -> &str {
        self.message
    }
    fn service(&self) -> &str {
        self.service
    }
    fn host(&self) -> &str {
        self.host
    }
    fn source(&self) -> &str {
        self.source
    }
    fn severity(&self) -> u8 {
        self.severity
    }
    fn attr(&self, key: &str) -> Option<&str> {
        self.attrs().find(|(k, _)| *k == key).map(|(_, v)| v)
    }
    fn trace_id(&self) -> Option<[u8; 16]> {
        self.trace_id
    }
    fn span_id(&self) -> Option<[u8; 8]> {
        self.span_id
    }
}

impl Compiled {
    /// The rules applied to a record's text: `false` when it goes.
    pub(crate) fn keep(&self, text: &mut String) -> bool {
        self.keep_into(text, &mut String::new())
    }

    /// As [`Compiled::keep`], a masked text written into `room` (the text's own room after, for
    /// the next): each match left to right the placeholder, as it is, as `replace_all` with
    /// `NoExpand` writes it.
    pub(crate) fn keep_into(&self, text: &mut String, room: &mut String) -> bool {
        for s in &self.steps {
            match s {
                Step::Exclude(r) if r.is_match(text) => return false,
                Step::Include(r) if !r.is_match(text) => return false,
                Step::Mask(r, with) => {
                    let mut last = None;
                    for m in r.find_iter(text) {
                        if last.is_none() {
                            room.clear();
                        }
                        room.push_str(text.get(last.unwrap_or(0)..m.start()).unwrap_or_default());
                        room.push_str(with);
                        last = Some(m.end());
                    }
                    if let Some(end) = last {
                        room.push_str(text.get(end..).unwrap_or_default());
                        std::mem::swap(text, room);
                    }
                }
                _ => {}
            }
        }
        true
    }

    /// Whether a line is read as JSON: its format's say, or (`auto`, a container's) an object's `{`.
    fn json(&self, text: &str) -> bool {
        match self.format {
            Format::Json => true,
            Format::Auto | Format::Cri | Format::Docker => text.trim_start().starts_with('{'),
            Format::Text => false,
        }
    }

    /// A line into `b`: one no active pipeline reads as its row, written from the line as it was
    /// read (a JSON line's fields from the room its reading keeps), nothing else made of it (the
    /// row its record would write: a test holds them alike), and its text back for the next
    /// line's; one a pipeline reads, or a service check's run, as its record
    /// ([`Compiled::record`]).
    pub(crate) fn write<M>(&self, line: Line, origin: &Origin, now_ns: i64, b: &mut Batch<M>, filters: &mut PipelineFilters) -> Option<String> {
        if self.source == kiln_obs::monitor::checks::SOURCE {
            b.push(self.record(line, origin, now_ns));
            return None;
        }
        let json = self.json(&line.text);
        let mut line = line;
        let mut text = std::mem::take(&mut line.text);
        with_room(|room| {
            let read = if json { self.json_fields(&mut text, room) } else { None };
            let room = &*room;
            let view = LineView {
                // As its record keeps it (`cloud::message`), so a filter reads what a record's would.
                message: kiln_obs::logs::record::cut(&text, kiln_obs::logs::record::MAX_MESSAGE_BYTES),
                service: self.service_of(read.as_ref(), &room.service, origin),
                host: &self.host,
                source: &self.source,
                severity: severity(read.as_ref(), &text),
                fields: read.as_ref().map(|_| &room.attrs),
                origin: &origin.attrs,
                stream: line.stream,
                truncated: line.truncated,
                tags: &self.tags,
                trace_id: read.as_ref().and_then(|j| j.trace_id),
                span_id: read.as_ref().and_then(|j| j.span_id),
            };
            let PipelineFilters { pipelines, scratch } = filters;
            if pipelines.iter().any(|p| p.reads(&view, scratch)) {
                b.push(self.record_of(text, &line, read.as_ref(), room, origin, now_ns));
                return None;
            }
            let ts = read.as_ref().and_then(|j| j.ts).or(line.ts).unwrap_or(now_ns);
            let ts = if ts > now_ns + AHEAD_MS * 1_000_000 { now_ns } else { ts };
            // An error's issue, as `kiln_obs::errors::put_log_issue` gives a record one: one it came
            // with left out, its own put last.
            let error = view.severity >= 17;
            let issue = error.then(|| kiln_obs::errors::log_issue(view.service, view.severity, &kiln_obs::errors::Error::of(view.attrs()))).flatten().map(kiln_obs::errors::hex);
            let kept = |(k, _): &(&str, &str)| !error || *k != "issue.id";
            let count = view.attrs().filter(kept).count() + usize::from(issue.is_some());
            let attrs = view.attrs().filter(kept).chain(issue.as_deref().map(|id| ("issue.id", id)));
            b.push_row(ts, view.severity, [view.service, view.host, view.source], view.message, attrs, count, (view.trace_id, view.span_id));
            Some(text)
        })
    }

    /// A line's record: a JSON line's message, level, time, trace and fields, or a text line's
    /// text and the level it starts with.
    ///
    /// A JSON line is read in place (`kiln_obs::json`): no tree of it is made, its fields borrowed
    /// from it until each is an attribute, the lists that hold them and its attributes' text kept
    /// from line to line, its message written into the line's own room.
    pub(crate) fn record(&self, line: Line, origin: &Origin, now_ns: i64) -> Record {
        let json = self.json(&line.text);
        let mut line = line;
        let mut text = std::mem::take(&mut line.text);
        with_room(|room| {
            let read = if json { self.json_fields(&mut text, room) } else { None };
            self.record_of(text, &line, read.as_ref(), room, origin, now_ns)
        })
    }

    /// The service a line's record names: the source's, else its container's, else one a JSON
    /// line names (`named`, as `read` says), else the source's name.
    fn service_of<'a>(&'a self, read: Option<&JsonRead>, named: &'a str, origin: &'a Origin) -> &'a str {
        match read {
            Some(j) if j.service => named,
            _ => self.service.as_deref().or(origin.service.as_deref()).unwrap_or(&self.name),
        }
    }

    /// A line's record of what its reading found (`read`, its fields in `room`; none, a text
    /// line's), `text` its message, `line` the rest of it.
    fn record_of(&self, text: String, line: &Line, read: Option<&JsonRead>, room: &JsonRoom, origin: &Origin, now_ns: i64) -> Record {
        let mut r = Record {
            ts: read.and_then(|j| j.ts).or(line.ts).unwrap_or(now_ns),
            severity: severity(read, &text),
            service: self.service_of(read, &room.service, origin).to_owned(),
            host: self.host.clone(),
            source: self.source.clone(),
            trace_id: read.and_then(|j| j.trace_id),
            span_id: read.and_then(|j| j.span_id),
            ..Record::default()
        };
        if r.ts > now_ns + AHEAD_MS * 1_000_000 {
            r.ts = now_ns;
        }
        // Its attributes sized once: the line's fields, where it came from, the source's tags.
        let fields = read.map_or(0, |_| room.attrs.len());
        r.attrs.reserve_exact(fields + origin.attrs.len() + self.tags.len() + 2);
        if read.is_some() {
            r.attrs.extend(room.attrs.iter().map(|(k, v)| (k.to_owned(), v.to_owned())));
        }
        r.attrs.extend(origin.attrs.iter().cloned());
        if let Some(s) = line.stream {
            r.attrs.push(("log.iostream".into(), s.into()));
        }
        if line.truncated {
            r.attrs.push(("truncated".into(), "true".into()));
        }
        r.attrs.extend(self.tags.iter().cloned());
        r.message = super::cloud::message(text);
        r
    }

    /// A JSON line read into `room` (its fields flattened, a service it names), `text` left its
    /// message (the line whole where it names none): what it says beside its fields; none when it
    /// is not an object.
    fn json_fields(&self, text: &mut String, room: &mut JsonRoom) -> Option<JsonRead> {
        let JsonRoom { levels, scratch, text: msg, key: buf, path, attrs, service } = room;
        attrs.clear();
        let mut pool = levels_for(std::mem::take(levels));
        pool.resize_with(JSON_DEPTH, Vec::new);
        let read = (|| {
            let (top, nested) = pool.split_first_mut()?;
            kiln_obs::json::object(text.as_str(), top, scratch).ok()?;
            let find = |keys: &[&str], string: bool| keys.iter().find_map(|k| top.iter().position(|(key, v)| (!string || matches!(v, Val::Str(_))) && key.is(k)));
            let value = |i: Option<usize>| i.and_then(|i| top.get(i)).map(|f| f.1);
            // Each of these taken out of the fields when it reads as one; else left a field.
            let mut taken = [usize::MAX; 3];
            let message = find(&MESSAGE_KEYS, true);
            msg.clear();
            if let (Some(i), Some(Val::Str(m))) = (message, value(message)) {
                msg.push_str(m.text(buf));
                taken[0] = i;
            }
            let lv = find(&LEVEL_KEYS, false);
            let level = value(lv).and_then(|v| level_of(v, buf));
            if let (Some(i), Some(_)) = (lv, level) {
                taken[1] = i;
            }
            let at = find(&TIME_KEYS, false);
            let mut ts = None;
            if let (Some(i), Some(t)) = (at, value(at).and_then(|v| time_of(v, buf))) {
                taken[2] = i;
                ts = Some(t);
            }
            let mut named = false;
            if self.service.is_none() {
                let field = find(&["service"], false).or_else(|| find(&["service.name"], false));
                if let Some(Val::Str(s)) = value(field)
                    && !s.raw.is_empty()
                {
                    service.clear();
                    service.push_str(s.text(buf));
                    named = true;
                }
            }
            let trace_id = TRACE_KEYS.iter().find_map(|k| match value(find(&[k], false)) {
                Some(Val::Str(s)) => hex_id::<16>(s.text(buf)),
                _ => None,
            });
            let span_id = SPAN_KEYS.iter().find_map(|k| match value(find(&[k], false)) {
                Some(Val::Str(s)) => hex_id::<8>(s.text(buf)),
                _ => None,
            });
            for (i, &(k, v)) in top.iter().enumerate() {
                if taken.contains(&i) || SKIPPED.iter().any(|s| k.is(s)) {
                    continue;
                }
                path.clear();
                path.push_str(k.text(buf));
                flatten(path, v, 1, nested, scratch, buf, attrs);
            }
            let found = taken[0] != usize::MAX;
            let level = match level {
                Some(l) => Level::Said(l),
                None if found => Level::Message,
                // The line its own message: what `severity_of` reads of a JSON message, the first
                // of its fields that is a string, read as a level or not.
                None => Level::Whole(
                    super::cloud::LEVEL_FIELDS
                        .iter()
                        .find_map(|k| match value(find(&[k], true)) {
                            Some(Val::Str(s)) => Some(s),
                            _ => None,
                        })
                        .and_then(|s| kiln_obs::logs::record::severities(s.text(buf)).map(|r| *r.start())),
                ),
            };
            Some((found, JsonRead { level, ts, service: named, trace_id, span_id }))
        })();
        *levels = levels_for(pool);
        let (found, read) = read?;
        if found {
            // The message into the line's own room: no longer than the line.
            text.clear();
            text.push_str(msg);
        }
        Some(read)
    }
}

/// What a JSON line says beside its fields (its room holds those, and a service it names).
#[derive(Debug, Clone, Copy)]
struct JsonRead {
    level: Level,
    ts: Option<i64>,
    /// It names its service (`JsonRoom::service`).
    service: bool,
    trace_id: Option<[u8; 16]>,
    span_id: Option<[u8; 8]>,
}

/// A line's level: a JSON line's as its reading says ([`Level`]), else the one its text starts
/// with.
fn severity(read: Option<&JsonRead>, text: &str) -> u8 {
    match read.map(|j| j.level) {
        Some(Level::Said(l)) => l,
        // A JSON line that names no message is its own: its level the first of its fields' that
        // `severity_of` reads a JSON message's from, found already, else its words'.
        Some(Level::Whole(l)) => l.unwrap_or_else(|| super::cloud::severity_of_words(text)),
        Some(Level::Message) | None => super::cloud::severity_of(text),
    }
}

/// A JSON line's attributes as its reading flattens them, in one text kept from line to line:
/// each key and its value side by side, where they end noted (nothing allocated a line once it
/// has grown).
#[derive(Default)]
struct Flat {
    text: String,
    /// Each attribute's key's start, its value's start (the key's end) and its value's end.
    spans: Vec<(usize, usize, usize)>,
}

impl Flat {
    fn clear(&mut self) {
        self.text.clear();
        self.spans.clear();
    }

    fn len(&self) -> usize {
        self.spans.len()
    }

    /// An attribute: `key`, and its value as `value` writes it after the key.
    fn push(&mut self, key: &str, value: impl FnOnce(&mut String)) {
        let k = self.text.len();
        self.text.push_str(key);
        let v = self.text.len();
        value(&mut self.text);
        self.spans.push((k, v, self.text.len()));
    }

    fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.spans.iter().map(|&(k, v, e)| (self.text.get(k..v).unwrap_or_default(), self.text.get(v..e).unwrap_or_default()))
    }
}

/// How a JSON line's level is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    /// A field said it.
    Said(u8),
    /// None said it; it names its message: the message's own.
    Message,
    /// None said it, nor names its message: the line's own, as a message (the level a field that
    /// `severity_of` reads gives, else its words').
    Whole(Option<u8>),
}

/// Containers a JSON line's fields are flattened from, at the most: the line's object and three
/// within (a field nested deeper is its JSON).
const JSON_DEPTH: usize = 4;
/// The keys a JSON line's message, level and time are read from: the first of each that is there.
const MESSAGE_KEYS: [&str; 7] = ["message", "msg", "log", "MESSAGE", "text", "@m", "@mt"];
const LEVEL_KEYS: [&str; 8] = ["level", "severity", "levelname", "log.level", "status", "lvl", "loglevel", "@l"];
const TIME_KEYS: [&str; 9] = ["timestamp", "time", "@timestamp", "ts", "date", "_timestamp", "Timestamp", "eventTime", "@t"];
const TRACE_KEYS: [&str; 3] = ["trace_id", "traceId", "trace.id"];
const SPAN_KEYS: [&str; 3] = ["span_id", "spanId", "span.id"];
/// Fields read into a record's own and never attributes.
const SKIPPED: [&str; 8] = ["trace_id", "traceId", "trace.id", "span_id", "spanId", "span.id", "service", "service.name"];

use kiln_obs::json::{Str, Val};

/// Room a JSON line's reading keeps from line to line: a list of fields for each depth, the
/// reader's, a message and a key decoded, a dotted key's path, the line's attributes and a service
/// it names.
#[derive(Default)]
struct JsonRoom {
    levels: Vec<Vec<(Str<'static>, Val<'static>)>>,
    scratch: kiln_obs::json::Scratch,
    text: String,
    key: String,
    path: String,
    attrs: Flat,
    service: String,
}

/// Lists of fields emptied, as lists of borrows of another lifetime, their room kept (collected in
/// place: the same layout, nothing allocated).
fn levels_for<'b>(v: Vec<Vec<(Str<'_>, Val<'_>)>>) -> Vec<Vec<(Str<'b>, Val<'b>)>> {
    v.into_iter()
        .map(|mut l| {
            l.clear();
            l.into_iter().map(|_| (Str { raw: "", escaped: false }, Val::Null)).collect()
        })
        .collect()
}

/// Runs `f` with this thread's room for JSON lines, or a new one were it in use.
fn with_room<T>(f: impl FnOnce(&mut JsonRoom) -> T) -> T {
    thread_local! {
        static ROOM: std::cell::RefCell<JsonRoom> = std::cell::RefCell::new(JsonRoom::default());
    }
    ROOM.with(|c| match c.try_borrow_mut() {
        Ok(mut room) => f(&mut room),
        Err(_) => f(&mut JsonRoom::default()),
    })
}

/// A level as JSON logs give it: a name, or a number (bunyan's and pino's 10–60, syslog's 0–7).
fn level_of(v: Val<'_>, buf: &mut String) -> Option<u8> {
    match v {
        Val::Str(s) => kiln_obs::logs::record::severities(s.text(buf).trim()).map(|r| *r.start()),
        Val::Num(n) => Some(match n.parse::<u64>().ok()? {
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
fn time_of(v: Val<'_>, buf: &mut String) -> Option<i64> {
    match v {
        Val::Str(s) => super::cloud::parse_time_ns(s.text(buf)),
        Val::Num(n) => {
            // Whole numbers exactly; fractions (seconds with their decimals) as floats.
            if let Some(i) = n.parse::<i64>().ok().filter(|&i| i > 0) {
                return match i {
                    ..100_000_000_000 => i.checked_mul(1_000_000_000),
                    100_000_000_000..100_000_000_000_000 => i.checked_mul(1_000_000),
                    100_000_000_000_000..100_000_000_000_000_000 => i.checked_mul(1_000),
                    _ => Some(i),
                };
            }
            let f = n.parse::<f64>().ok()?;
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

/// `s`, at most the longest value an attribute holds, cut at a character.
fn cut_str(s: &str) -> &str {
    if s.len() <= VALUE_MAX {
        return s;
    }
    let mut c = VALUE_MAX;
    while !s.is_char_boundary(c) {
        c -= 1;
    }
    s.get(..c).unwrap_or("")
}

/// A JSON line's field as attributes (its key `path`): nested objects' keys joined by dots (their
/// fields read into the first of `pool`'s lists, deeper ones into the rest), null left out, a
/// string decoded, a number as written, an array or an object nested too deep as its JSON without
/// its spaces.
#[allow(clippy::too_many_arguments)]
fn flatten<'a>(path: &mut String, v: Val<'a>, depth: usize, pool: &mut [Vec<(Str<'a>, Val<'a>)>], scratch: &mut kiln_obs::json::Scratch, buf: &mut String, out: &mut Flat) {
    if out.len() >= ATTRS_MAX {
        return;
    }
    match v {
        Val::Object(raw) if depth < JSON_DEPTH => {
            let Some((list, rest)) = pool.split_first_mut() else { return };
            // Checked whole with the line already: it reads.
            if kiln_obs::json::object_at(raw, depth + 1, list, scratch).is_err() {
                return;
            }
            let at = path.len();
            for &(k, v) in list.iter() {
                path.truncate(at);
                path.push('.');
                path.push_str(k.text(buf));
                flatten(path, v, depth + 1, rest, scratch, buf, out);
            }
            path.truncate(at);
        }
        Val::Null => {}
        // A stack whole: its row keeps its ends (`kiln_obs::errors::stack_ends`).
        Val::Str(s) if kiln_obs::errors::STACK_KEYS.contains(&path.as_str()) => out.push(path, |t| t.push_str(s.text(buf))),
        Val::Str(s) => out.push(path, |t| t.push_str(cut_str(s.text(buf)))),
        Val::Num(n) => out.push(path, |t| t.push_str(n)),
        Val::Bool(b) => out.push(path, |t| t.push_str(if b { "true" } else { "false" })),
        // As its JSON, compact, cut as a value is.
        Val::Array(raw) | Val::Object(raw) => out.push(path, |t| {
            let at = t.len();
            kiln_obs::json::compact(raw, t);
            let kept = t.get(at..).map_or(0, |v| cut_str(v).len());
            t.truncate(at + kept);
        }),
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

/// Where records go (records, and records made rows where they were read): offered with an ack;
/// refused (no room), they come back, to be offered again.
pub(crate) type Offer<'a> = &'a (dyn Fn(Logs, Ack) -> Result<(), Logs> + Sync);

/// The observe owner's offer: what a machine's sources write through.
fn offer_to_observe(logs: Logs, ack: Ack) -> Result<(), Logs> {
    match super::api::get() {
        Some(o) => o.offer_log_batch(logs, ack),
        None => Err(logs),
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
        Kind::UnifiedLog => unified::sample(&c, 20),
        Kind::WindowsEvent => winevent::sample(&c, 20),
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
        "ok": sampled.error.is_none() && (matches!(c.kind, Kind::Journald | Kind::Syslog | Kind::UnifiedLog | Kind::WindowsEvent) || sampled.matched > 0),
        "files": sampled.files.iter().map(|(p, b)| serde_json::json!({ "path": p, "bytes": b })).collect::<Vec<_>>(),
        "matched": sampled.matched,
        "records": records,
        "excluded": sampled.excluded,
        "error": sampled.error.or_else(|| (!matches!(c.kind, Kind::Journald | Kind::Syslog | Kind::UnifiedLog | Kind::WindowsEvent) && sampled.matched == 0).then(|| "no file here matches its paths".to_owned())),
        "took_ms": u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX),
    })
}

/// For benches (`benches/collect.rs`): every file `source` matches, read whole on this thread as a
/// machine reads it, its places kept in `places`, each offer written at once (`written`: through a
/// batcher into its writes): the records, the bytes read and the bytes written.
#[doc(hidden)]
pub fn read_whole(name: &str, source: &Source, places: &Path, written: bool) -> Result<(u64, u64, u64), String> {
    let c = source.compile(name, "bench")?;
    Ok(files::read_whole(&c, places, written))
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
                                Kind::UnifiedLog => unified::run(&c, &file, &stop2, &state2, &offer_to_observe),
                                Kind::WindowsEvent => winevent::run(&c, &file, &stop2, &state2, &offer_to_observe),
                                Kind::File | Kind::Kubernetes | Kind::Docker => files::run(&c, &file, &stop2, &state2, &offer_to_observe),
                                Kind::Syslog => syslog::run(c, &stop2, &state2, &(Arc::new(|r: Vec<Record>, a: Ack| offer_to_observe(r.into(), a).map_err(Logs::into_records)) as syslog::Sink)),
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

/// Lines a process reads ahead of what is offered at the most: past them, it waits on its pipe.
const LINES_AHEAD: usize = 4_096;
/// The longest line read of a process's output (`journalctl --all` prints a field whole); a longer
/// one is skipped.
const LINE_MAX: usize = 4 << 20;

/// A process whose output is read a line at a time (`journalctl`, macOS's `log`), the lines
/// coming through a channel (a thread reads its pipe), what it says on stderr kept apart.
pub(crate) struct Reading {
    pub child: std::process::Child,
    pub lines: std::sync::mpsc::Receiver<String>,
    pub errors: std::thread::JoinHandle<String>,
}

/// Starts `cmd`, its output read a line at a time (`what` names its threads).
pub(crate) fn read_lines(cmd: &mut std::process::Command, what: &str) -> std::io::Result<Reading> {
    use std::io::{BufRead as _, Read as _};
    use std::process::Stdio;
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let (tx, rx) = std::sync::mpsc::sync_channel::<String>(LINES_AHEAD);
    let out = child.stdout.take().ok_or_else(|| std::io::Error::other("no stdout"))?;
    let err = child.stderr.take().ok_or_else(|| std::io::Error::other("no stderr"))?;
    std::thread::Builder::new().name(format!("kiln-{what}-read")).spawn(move || {
        let mut r = std::io::BufReader::with_capacity(256 << 10, out);
        let mut line = Vec::new();
        loop {
            line.clear();
            match (&mut r).take(LINE_MAX as u64).read_until(b'\n', &mut line) {
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
    let errors = std::thread::Builder::new().name(format!("kiln-{what}-err")).spawn(move || {
        let mut s = String::new();
        let _ = err.take(64 << 10).read_to_string(&mut s);
        s
    })?;
    Ok(Reading { child, lines: rx, errors })
}

/// How long a process's output is waited for after it exits.
const AFTER_EXIT: Duration = Duration::from_millis(200);

/// Whether `r`'s process has ended and its output been read (or will not come): an exit, then no
/// line for [`AFTER_EXIT`]. Its output's end is not waited for alone: on macOS a process another
/// thread starts as the pipe is made may hold its writing end open (the pipe is made, then marked
/// close-on-exec, in two steps), and the end never comes.
pub(crate) fn ended(r: &mut Reading, quiet_since: &mut Option<Instant>) -> bool {
    if !matches!(r.child.try_wait(), Ok(Some(_))) {
        *quiet_since = None;
        return false;
    }
    quiet_since.get_or_insert_with(Instant::now).elapsed() >= AFTER_EXIT
}

/// Runs `cmd` to its end, `within` at the most: its lines, its exit, and what it said on stderr.
pub(crate) fn output_lines(cmd: &mut std::process::Command, what: &str, within: Duration) -> std::io::Result<(Vec<String>, Option<std::process::ExitStatus>, String)> {
    let mut r = read_lines(cmd, what)?;
    let end = Instant::now() + within;
    let mut out = Vec::new();
    let mut quiet = None;
    loop {
        match r.lines.recv_timeout(Duration::from_millis(50)) {
            Ok(l) => {
                out.push(l);
                quiet = None;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if ended(&mut r, &mut quiet) {
                    break;
                }
                if Instant::now() >= end {
                    let _ = r.child.kill();
                    let _ = r.child.wait();
                    return Ok((out, None, String::new()));
                }
            }
        }
    }
    let status = r.child.wait().ok();
    // What it said, if its pipe has closed: not waited for (as above).
    let said = if r.errors.is_finished() { r.errors.join().unwrap_or_default() } else { String::new() };
    Ok((out, status, said))
}

/// Records gathered for one offer, and where each source they came from may move its place once
/// they are written.
pub(crate) struct Batch<M> {
    /// What it holds, in the order it was read: runs of records, and segments of rows (records
    /// written as rows already, [`Compiled::write`]); then the segment being written, its rows and
    /// its last row's time.
    pub parts: Vec<LogPart>,
    open: Vec<u8>,
    open_rows: u32,
    prev: i64,
    pub bytes: usize,
    pub marks: Vec<M>,
    /// Records through the pipelines already, in order: a refused offer's come back, not processed
    /// twice.
    pub processed: usize,
}

impl<M> Default for Batch<M> {
    fn default() -> Self {
        Self { parts: Vec::new(), open: Vec::new(), open_rows: 0, prev: 0, bytes: 0, marks: Vec::new(), processed: 0 }
    }
}

/// A segment of rows a batch writes records into: a log block's text, past which a new one begins.
const ROW_SEGMENT: usize = kiln_obs::logs::block::BLOCK_TEXT;
/// Room a segment begins with: some hundred lines' rows.
const ROW_SEGMENT_START: usize = 16 << 10;

impl<M> Batch<M> {
    pub(crate) fn push(&mut self, r: Record) {
        self.bytes += super::ingest::record_bytes(&r);
        // After the rows before it, in order.
        self.seal();
        match self.parts.last_mut() {
            Some(LogPart::Records(run)) => run.push(r),
            _ => self.parts.push(LogPart::Records(vec![r])),
        }
    }

    /// A record written as its row (a line no pipeline reads), into the segment being written.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn push_row<'a>(
        &mut self,
        ts: i64,
        severity: u8,
        names: [&str; 3],
        message: &str,
        attrs: impl Iterator<Item = (&'a str, &'a str)>,
        attr_count: usize,
        ids: (Option<[u8; 16]>, Option<[u8; 8]>),
    ) {
        if self.open.capacity() == 0 {
            self.open.reserve(ROW_SEGMENT_START);
        }
        let before = self.open.len();
        kiln_obs::logs::record::put_fields(&mut self.open, ts, severity, names, message, attrs, attr_count, ids.0, ids.1, self.prev, kiln_obs::batch::MAX_NAME_BYTES);
        self.prev = ts;
        self.open_rows += 1;
        self.bytes += self.open.len() - before;
        if self.open.len() >= ROW_SEGMENT {
            self.seal();
        }
    }

    /// The segment being written, ended (the next row begins another, its time chained from 0).
    fn seal(&mut self) {
        if self.open_rows > 0 {
            self.parts.push(LogPart::Rows { raw: std::mem::take(&mut self.open), rows: self.open_rows, last_ts: self.prev });
            self.open_rows = 0;
            self.prev = 0;
        }
    }

    /// Records, whichever form.
    pub(crate) fn len(&self) -> usize {
        let held: usize = self
            .parts
            .iter()
            .map(|p| match p {
                LogPart::Records(r) => r.len(),
                LogPart::Rows { rows, .. } => *rows as usize,
            })
            .sum();
        held + self.open_rows as usize
    }

    /// Every record it holds, in order, rows read back as records (what a source's sample shows).
    pub(crate) fn take_records(&mut self) -> Vec<Record> {
        self.seal();
        self.bytes = 0;
        self.processed = 0;
        let mut out = Vec::new();
        for part in std::mem::take(&mut self.parts) {
            match part {
                LogPart::Records(r) => out.extend(r),
                LogPart::Rows { raw, rows, .. } => {
                    let mut rows = kiln_obs::logs::record::Rows::new(&raw, rows as usize);
                    while let Ok(Some(r)) = rows.next_row() {
                        out.push(r.to_owned());
                    }
                }
            }
        }
        out
    }

    pub(crate) fn full(&self) -> bool {
        self.len() >= BATCH_RECORDS || self.bytes >= BATCH_BYTES
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.parts.is_empty() && self.open_rows == 0 && self.marks.is_empty()
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
        b.seal();
        let ack = Ack::new();
        let n = b.len() as u64;
        // Through the pipelines once, the first time they are offered (rows are of lines none
        // reads): each run's records past those processed before.
        let mut seen = 0usize;
        for part in &mut b.parts {
            if let LogPart::Records(run) = part {
                if let Some(fresh) = run.get_mut(b.processed.saturating_sub(seen)..) {
                    super::pipeline::kept::process(fresh);
                }
                seen += run.len();
            }
        }
        b.processed = seen;
        match offer(Logs { parts: std::mem::take(&mut b.parts) }, ack.clone()) {
            Ok(()) => {
                self.queue.push_back((ack, std::mem::take(&mut b.marks), n));
                b.bytes = 0;
                b.processed = 0;
                true
            }
            Err(back) => {
                b.parts = back.parts;
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

    /// A line no pipeline reads, written as its row, is the row its record writes (with the issue
    /// the owner gives a record of an error's), to the byte: a text line whatever it holds (a
    /// level's word, an `issue.id` tag on an error's line, characters of every width, a line past
    /// a record's message, a time ahead of now), a JSON line whatever its fields (nested, escaped,
    /// its message, level, time, service and ids, broken JSON read as text), its stream, its being
    /// cut short, its origin's attributes and service; its text back for the next line's. With
    /// kiln's nginx pipeline on, a line it reads is its record still, and another source's a row.
    #[test]
    fn a_line_written_as_its_row_is_its_record_s_row() -> anyhow::Result<()> {
        const NOW: i64 = 1_792_324_800_000_000_000;
        let plain = file_source().compile("app", "web-1").map_err(anyhow::Error::msg)?;
        let tagged = Source { tags: vec!["env:prod".into(), "issue.id:0123".into(), "team:shop".into()], service: None, source: Some("app".into()), ..file_source() }
            .compile("shop", "web-1")
            .map_err(anyhow::Error::msg)?;
        let mut rng = kiln_obs::testkit::Rng::new(12);
        let words = ["ERROR", "error:", "[WARN]", "info", "connection", "refused", "é", "漢字", "🙂", "\t", "  ", "{", "level=error", "panic:", "x"];
        let (mut rows, mut json) = (0, 0);
        for i in 0..8000 {
            let c = if i % 2 == 0 { &plain } else { &tagged };
            let n = if rng.below(50) == 0 { 30_000 } else { rng.below(12) as usize };
            let text: String = if i % 4 < 2 { (0..n).map(|_| words.get(rng.below(words.len() as u64) as usize).copied().unwrap_or("")).collect::<Vec<_>>().join(" ") } else { log_line(&mut rng) };
            let ts = [None, Some(NOW - 5_000_000_000), Some(NOW + 3_600_000_000_000)].get(rng.below(3) as usize).copied().flatten();
            let stream = [None, Some("stdout"), Some("stderr")].get(rng.below(3) as usize).copied().flatten();
            let truncated = rng.below(5) == 0;
            let origin = Origin { attrs: vec![("log.file.path".into(), "/var/log/app/a.log".into())], service: (rng.below(2) == 0).then(|| "pod-svc".to_owned()) };
            let line = || Line { text: text.clone(), ts, stream, truncated };
            let mut b: Batch<()> = Batch::default();
            let back = c.write(line(), &origin, NOW, &mut b, &mut PipelineFilters::default());
            b.seal();
            // A JSON line's text comes back its message.
            let object = c.json(&text) && serde_json::from_str::<serde_json::Map<String, Value>>(&text).is_ok();
            anyhow::ensure!(back.is_some() && (object || back.as_deref() == Some(text.as_str())), "a row's text comes back: {text:?}");
            json += usize::from(object);
            let mut r = c.record(line(), &origin, NOW);
            kiln_obs::errors::put_log_issue(&mut r);
            let mut want = Vec::new();
            kiln_obs::logs::record::put_row(&mut want, &r, 0);
            anyhow::ensure!(matches!(b.parts.as_slice(), [LogPart::Rows { raw, rows: 1, last_ts }] if *raw == want && *last_ts == r.ts), "{text:?}");
            rows += 1;
        }
        anyhow::ensure!(rows == 8000 && json > 1000, "{rows} rows, {json} of JSON lines");
        let pipelines: Vec<_> = crate::observe::pipeline::builtin::all().into_iter().filter_map(|(_, d)| d.compile().ok()).collect();
        let mut filters = PipelineFilters { pipelines: Arc::new(pipelines), ..PipelineFilters::default() };
        let nginx = Source { source: Some("nginx".into()), ..file_source() }.compile("nginx", "web-1").map_err(anyhow::Error::msg)?;
        let mut b: Batch<()> = Batch::default();
        let access = r#"192.0.2.10 - - [18/Oct/2026:12:00:00 +0000] "GET / HTTP/1.1" 200 1 "-" "curl/8.5.0""#;
        anyhow::ensure!(nginx.write(Line { text: access.into(), ..Line::default() }, &Origin::default(), NOW, &mut b, &mut filters).is_none(), "a record keeps its text");
        anyhow::ensure!(plain.write(Line { text: "an app's line".into(), ..Line::default() }, &Origin::default(), NOW, &mut b, &mut filters).is_some(), "a row's text comes back");
        anyhow::ensure!(
            nginx.write(Line { text: r#"{"msg":"an app's JSON","level":"warn"}"#.into(), ..Line::default() }, &Origin::default(), NOW, &mut b, &mut filters).is_none(),
            "a pipeline reads it"
        );
        anyhow::ensure!(plain.write(Line { text: r#"{"msg":"an app's JSON","level":"warn"}"#.into(), ..Line::default() }, &Origin::default(), NOW, &mut b, &mut filters).is_some(), "none reads it");
        b.seal();
        anyhow::ensure!(
            matches!(b.parts.as_slice(), [LogPart::Records(r), LogPart::Rows { rows: 1, .. }, LogPart::Records(j), LogPart::Rows { rows: 1, .. }] if r.len() == 1 && j.len() == 1),
            "{:?}",
            b.parts
        );
        Ok(())
    }

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
            predicate: None,
            level: None,
            channels: Vec::new(),
            query: None,
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
        // Masked into a room kept from line to line, as `replace_all` with `NoExpand` masks:
        // matches at either end, next to each other, empty ones, across characters of every width.
        let mut rng = kiln_obs::testkit::Rng::new(5);
        let parts = ["GET ", "token=a1", "x", "", "é", "漢", "🙂", "$1", "token=", " costs ", "aa"];
        let mut room = String::new();
        for pattern in ["token=[A-Za-z0-9]+", "x*", "a", "é|漢", "^GET", "\\d+$", "(?:aa)+"] {
            let s = Source { rules: vec![rule(RuleKind::MaskSequences, pattern, Some("[m]$1"))], ..file_source() };
            let c = s.compile("app", "h").map_err(anyhow::Error::msg)?;
            let re = regex::Regex::new(pattern)?;
            for _ in 0..300 {
                let text: String = (0..rng.below(8)).filter_map(|_| parts.get(rng.below(parts.len() as u64) as usize).copied()).collect();
                let mut t = text.clone();
                anyhow::ensure!(c.keep_into(&mut t, &mut room));
                anyhow::ensure!(t == re.replace_all(&text, regex::NoExpand("[m]$1")), "{pattern} {text:?}: {t:?}");
            }
        }
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
        // No message named, nor a level its first level field reads as: the line its message, its
        // level the first of its fields a JSON message's is read from that is a string.
        let r = c.record(Line { text: r#"{"level":{"n":1},"severity":"error","route":"/a"}"#.into(), ..Line::default() }, &origin, 1_800_000_000_000_000_000);
        anyhow::ensure!(r.severity == 17 && r.message.starts_with('{') && r.attrs.iter().any(|(k, v)| k == "level.n" && v == "1"), "{r:?}");
        Ok(())
    }

    /// The record of a line as kiln read it before, `serde_json`'s tree of the line made: what the
    /// reading in place is held to, field for field.
    fn reference(c: &Compiled, line: Line, origin: &Origin, now_ns: i64) -> Record {
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
        fn time_ns(v: &Value) -> Option<i64> {
            match v {
                Value::String(s) => crate::observe::cloud::parse_time_ns(s),
                Value::Number(n) => {
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
        let parse = match c.format {
            Format::Json => true,
            Format::Auto | Format::Cri | Format::Docker => line.text.trim_start().starts_with('{'),
            Format::Text => false,
        };
        let object = if parse { serde_json::from_str::<Value>(&line.text).ok().and_then(|v| if let Value::Object(o) = v { Some(o) } else { None }) } else { None };
        let mut r = Record {
            ts: line.ts.unwrap_or(now_ns),
            service: c.service.clone().or_else(|| origin.service.clone()).unwrap_or_else(|| c.name.clone()),
            host: c.host.clone(),
            source: c.source.clone(),
            ..Record::default()
        };
        match object {
            Some(mut o) => {
                let message = ["message", "msg", "log", "MESSAGE", "text", "@m", "@mt"].iter().find_map(|k| if let Some(Value::String(_)) = o.get(*k) { o.remove(*k) } else { None });
                r.message = match message {
                    Some(Value::String(m)) => m,
                    _ => line.text.clone(),
                };
                let first = |o: &serde_json::Map<String, Value>, keys: &[&'static str]| keys.iter().copied().find(|k| o.contains_key(*k));
                r.severity = match first(&o, &["level", "severity", "levelname", "log.level", "status", "lvl", "loglevel", "@l"]) {
                    Some(k) if let Some(l) = o.get(k).and_then(level) => {
                        o.remove(k);
                        l
                    }
                    _ => crate::observe::cloud::severity_of(&r.message),
                };
                if let Some(k) = first(&o, &["timestamp", "time", "@timestamp", "ts", "date", "_timestamp", "Timestamp", "eventTime", "@t"])
                    && let Some(ts) = o.get(k).and_then(time_ns)
                {
                    o.remove(k);
                    r.ts = ts;
                }
                if c.service.is_none()
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
                r.severity = crate::observe::cloud::severity_of(&line.text);
                r.message = line.text;
            }
        }
        r.message = crate::observe::cloud::message(std::mem::take(&mut r.message));
        if r.ts > now_ns + AHEAD_MS * 1_000_000 {
            r.ts = now_ns;
        }
        r.attrs.extend(origin.attrs.iter().cloned());
        r.attrs.extend(c.tags.iter().cloned());
        r
    }

    /// One of `of`, at random.
    fn pick<'a>(rng: &mut kiln_obs::testkit::Rng, of: &[&'a str]) -> &'a str {
        of.get(rng.below(of.len() as u64) as usize).copied().unwrap_or_default()
    }

    /// A random JSON log line: its message, level, time, service and ids under any of their names
    /// (of any type), other fields nested past the depth flattened, arrays, escapes, a key twice;
    /// now and then not JSON, or JSON broken.
    fn log_line(rng: &mut kiln_obs::testkit::Rng) -> String {
        fn string(rng: &mut kiln_obs::testkit::Rng, out: &mut String) {
            out.push('"');
            for _ in 0..rng.below(6) {
                out.push_str(pick(rng, &["a", "é", "😀", "\\n", "\\\"", "\\\\", "\\/", "\\u00e9", "\\ud83d\\ude00", " ", "ERROR", "warn"]));
            }
            out.push('"');
        }
        fn value(rng: &mut kiln_obs::testkit::Rng, depth: u32, out: &mut String) {
            match rng.below(if depth == 0 { 5 } else { 7 }) {
                0 | 4 => string(rng, out),
                1 => out.push_str(pick(rng, &["0", "-0", "12", "50", "3", "-7.25", "1e3", "2.5E-4", "123456789012345678901234", "1792324800500", "1792324800.5", "1.0"])),
                2 => out.push_str(pick(rng, &["true", "false"])),
                3 => out.push_str("null"),
                5 => {
                    out.push('[');
                    for i in 0..rng.below(3) {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        value(rng, depth - 1, out);
                    }
                    out.push(']');
                }
                _ => object(rng, depth - 1, out),
            }
        }
        fn object(rng: &mut kiln_obs::testkit::Rng, depth: u32, out: &mut String) {
            const KEYS: &str = "message msg log text level severity status lvl time timestamp ts @t service service.name trace_id traceId span_id route user dup a.b \\u006dsg";
            let keys: Vec<&str> = KEYS.split(' ').collect();
            out.push('{');
            for i in 0..rng.below(7) {
                if i > 0 {
                    out.push_str(pick(rng, &[",", " , "]));
                }
                out.push('"');
                out.push_str(pick(rng, &keys));
                out.push_str("\":");
                match rng.below(8) {
                    0 => out.push_str(pick(rng, &["\"0af7651916cd43dd8448eb211c80319c\"", "\"b7ad6b7169203331\"", "\"2026-10-18T12:00:00.5Z\"", "\"info\"", "\"error\""])),
                    _ => value(rng, depth, out),
                }
            }
            out.push('}');
        }
        let mut s = String::new();
        match rng.below(10) {
            0 => s.push_str("2026-10-18 12:00:00 ERROR payment failed"),
            1 => {
                object(rng, 5, &mut s);
                s.pop();
            }
            _ => object(rng, 5, &mut s),
        }
        s
    }

    #[test]
    fn a_json_line_read_in_place_is_the_record_its_tree_made() -> anyhow::Result<()> {
        let mut rng = kiln_obs::testkit::Rng::new(7);
        let origin = Origin { attrs: vec![("log.file.path".into(), "/var/log/app/a.log".into())], service: None };
        for named in [true, false] {
            let c = Source { service: named.then(|| "app".to_owned()), ..file_source() }.compile("app", "web-1").map_err(anyhow::Error::msg)?;
            for _ in 0..20_000 {
                let text = log_line(&mut rng);
                let now = 1_800_000_000_000_000_000;
                let theirs = reference(&c, Line { text: text.clone(), ..Line::default() }, &origin, now);
                let mine = c.record(Line { text: text.clone(), ..Line::default() }, &origin, now);
                anyhow::ensure!(
                    (mine.ts, mine.severity, &mine.service, &mine.host, &mine.source, &mine.message, mine.trace_id, mine.span_id)
                        == (theirs.ts, theirs.severity, &theirs.service, &theirs.host, &theirs.source, &theirs.message, theirs.trace_id, theirs.span_id),
                    "{text}\n{mine:?}\n{theirs:?}"
                );
                // A number as written (the tree's as serde_json writes the double it read: beyond a
                // double's digits, alike to its last place or two), an array as its JSON: alike as
                // JSON values.
                fn same(x: &Value, y: &Value) -> bool {
                    match (x, y) {
                        (Value::Number(p), Value::Number(q)) => p.as_f64().zip(q.as_f64()).is_some_and(|(p, q)| (p - q).abs() <= 1e-15 * p.abs().max(q.abs())),
                        (Value::Array(p), Value::Array(q)) => p.len() == q.len() && p.iter().zip(q).all(|(p, q)| same(p, q)),
                        (Value::Object(p), Value::Object(q)) => p.len() == q.len() && p.iter().all(|(k, v)| q.get(k).is_some_and(|w| same(v, w))),
                        _ => x == y,
                    }
                }
                let alike = |a: &str, b: &str| a == b || matches!((serde_json::from_str::<Value>(a), serde_json::from_str::<Value>(b)), (Ok(x), Ok(y)) if same(&x, &y));
                // In any order: the tree's was its map's (`IndexMap::remove` moved its last field into
                // the place of one taken out), this one's the line's.
                let (mut m, mut t) = (mine.attrs.clone(), theirs.attrs.clone());
                m.sort();
                t.sort();
                anyhow::ensure!(m.len() == t.len() && m.iter().zip(&t).all(|(a, b)| a.0 == b.0 && alike(&a.1, &b.1)), "{text}\n{m:?}\n{t:?}");
            }
        }
        Ok(())
    }
}
