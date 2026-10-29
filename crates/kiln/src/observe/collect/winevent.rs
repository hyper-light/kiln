//! The Windows Event Log (`docs/observe.md` §8.1), read through `wevtutil` (as Datadog's
//! `windows_event` source reads its channels): each channel the source names polled each second
//! for the events after the last one written, `wevtutil qe <channel> /q:"*[System[(EventRecordID >
//! N)]]" /f:RenderedXml`, its query (an XPath of the channel's events) joined to that; an event
//! read with its rendered message. A channel's place is its last event's record id, kept once its
//! record is written; a channel cleared (its ids begun again, below the place) is read from its
//! start. From now when no place is kept, or from the channel's start with `start: beginning`.
//! Where there is no `wevtutil` (Linux, macOS), its status says so.
//!
//! `KILN_WEVTUTIL` names another `wevtutil` (a test's).

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use kiln_obs::logs::record::Record;
use serde::{Deserialize, Serialize};

use super::{Batch, Compiled, InFlight, Offer, Start, State, Stop};

/// How often each channel is asked for what came.
const POLL: Duration = Duration::from_secs(1);
/// Events a channel is asked for at once.
const BATCH: usize = 500;
/// How long a call may take.
const CALL_MAX: Duration = Duration::from_secs(30);
/// How often the places are written down.
const SAVE_EVERY: Duration = Duration::from_secs(1);

/// An element of an event's XML: its name (without a namespace's prefix), attributes, text and
/// children.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct El {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub text: String,
    pub children: Vec<El>,
}

impl El {
    pub fn child(&self, name: &str) -> Option<&El> {
        self.children.iter().find(|c| c.name == name)
    }

    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// The text of the child at `path`.
    pub fn text_at(&self, path: &[&str]) -> Option<&str> {
        let mut e = self;
        for p in path {
            e = e.child(p)?;
        }
        Some(e.text.trim()).filter(|t| !t.is_empty())
    }
}

/// `&lt;` and the like, and character references, read.
fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(rest.get(..i).unwrap_or_default());
        rest = rest.get(i..).unwrap_or_default();
        let Some(end) = rest.find(';').filter(|&e| e <= 12) else {
            out.push('&');
            rest = rest.get(1..).unwrap_or_default();
            continue;
        };
        let ent = rest.get(1..end).unwrap_or_default();
        let c = match ent {
            "lt" => Some('<'),
            "gt" => Some('>'),
            "amp" => Some('&'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ if ent.starts_with("#x") || ent.starts_with("#X") => u32::from_str_radix(ent.get(2..).unwrap_or_default(), 16).ok().and_then(char::from_u32),
            _ if ent.starts_with('#') => ent.get(1..).unwrap_or_default().parse::<u32>().ok().and_then(char::from_u32),
            _ => None,
        };
        match c {
            Some(c) => {
                out.push(c);
                rest = rest.get(end + 1..).unwrap_or_default();
            }
            None => {
                out.push('&');
                rest = rest.get(1..).unwrap_or_default();
            }
        }
    }
    out.push_str(rest);
    out
}

/// A name without its namespace's prefix (`ev:Event` is `Event`).
fn local(name: &str) -> String {
    name.rsplit(':').next().unwrap_or(name).to_owned()
}

/// The top-level elements of `xml` (events, one after another, as `wevtutil qe` writes them): an
/// event's XML is well-formed, and what is not read as XML is skipped (a declaration, a comment).
pub(crate) fn parse(xml: &str) -> Vec<El> {
    let mut out = Vec::new();
    let mut stack: Vec<El> = Vec::new();
    let mut rest = xml;
    while let Some(lt) = rest.find('<') {
        if let Some(top) = stack.last_mut() {
            top.text.push_str(&unescape(rest.get(..lt).unwrap_or_default()));
        }
        rest = rest.get(lt..).unwrap_or_default();
        if let Some(r) = rest.strip_prefix("<!--") {
            rest = r.find("-->").and_then(|e| r.get(e + 3..)).unwrap_or_default();
            continue;
        }
        if let Some(r) = rest.strip_prefix("<![CDATA[") {
            let end = r.find("]]>").unwrap_or(r.len());
            if let Some(top) = stack.last_mut() {
                top.text.push_str(r.get(..end).unwrap_or_default());
            }
            rest = r.get(end + 3..).unwrap_or_default();
            continue;
        }
        if rest.starts_with("<?") || rest.starts_with("<!") {
            rest = rest.find('>').and_then(|e| rest.get(e + 1..)).unwrap_or_default();
            continue;
        }
        // A tag: up to its `>` outside a quoted value.
        let mut quote = None;
        let mut end = None;
        for (i, c) in rest.char_indices().skip(1) {
            match (quote, c) {
                (None, '"' | '\'') => quote = Some(c),
                (Some(q), c) if c == q => quote = None,
                (None, '>') => {
                    end = Some(i);
                    break;
                }
                _ => {}
            }
        }
        let Some(end) = end else { break };
        let tag = rest.get(1..end).unwrap_or_default();
        rest = rest.get(end + 1..).unwrap_or_default();
        if let Some(name) = tag.strip_prefix('/') {
            let name = local(name.trim());
            // Closes the innermost element of that name (a stray close is skipped).
            if let Some(pos) = stack.iter().rposition(|e| e.name == name) {
                while stack.len() > pos {
                    let Some(done) = stack.pop() else { break };
                    match stack.last_mut() {
                        Some(parent) => parent.children.push(done),
                        None => out.push(done),
                    }
                }
            }
            continue;
        }
        let (tag, closed) = match tag.strip_suffix('/') {
            Some(t) => (t, true),
            None => (tag, false),
        };
        let mut parts = tag.trim();
        let name_end = parts.find(char::is_whitespace).unwrap_or(parts.len());
        let mut el = El { name: local(parts.get(..name_end).unwrap_or_default()), ..El::default() };
        parts = parts.get(name_end..).unwrap_or_default();
        // Attributes: name="value" or name='value'.
        while let Some(eq) = parts.find('=') {
            let key = parts.get(..eq).unwrap_or_default().trim();
            let after = parts.get(eq + 1..).unwrap_or_default().trim_start();
            let Some(q) = after.chars().next().filter(|c| *c == '"' || *c == '\'') else { break };
            let body = after.get(1..).unwrap_or_default();
            let Some(close) = body.find(q) else { break };
            if !key.starts_with("xmlns") {
                el.attrs.push((local(key), unescape(body.get(..close).unwrap_or_default())));
            }
            parts = body.get(close + 1..).unwrap_or_default();
        }
        if closed {
            match stack.last_mut() {
                Some(parent) => parent.children.push(el),
                None => out.push(el),
            }
        } else {
            stack.push(el);
        }
    }
    out
}

/// A Windows event level's severity (OpenTelemetry's numbers): critical, error, warning,
/// information, verbose; `LogAlways` (0) information.
fn severity(level: Option<&str>) -> u8 {
    match level.and_then(|l| l.parse::<u8>().ok()) {
        Some(1) => 21,
        Some(2) => 17,
        Some(3) => 13,
        Some(5) => 5,
        _ => 9,
    }
}

/// `2026-10-19T11:01:27.1771960Z` (an event's `SystemTime`) as ns since the epoch.
fn system_time(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let n = |r: std::ops::Range<usize>| s.get(r).and_then(|x| x.parse::<i64>().ok());
    if b.get(4) != Some(&b'-') || b.get(10) != Some(&b'T') {
        return None;
    }
    let (y, mo, d, h, mi, sec) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    let mut frac = 0i64;
    if b.get(19) == Some(&b'.') {
        let digits: String = s.get(20..).unwrap_or_default().chars().take_while(char::is_ascii_digit).take(9).collect();
        if !digits.is_empty() {
            frac = digits.parse::<i64>().ok()? * 10i64.pow(9 - u32::try_from(digits.len()).ok()?);
        }
    }
    let secs = kiln_obs::query::calendar::days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec;
    secs.checked_mul(1_000_000_000)?.checked_add(frac)
}

/// An event's record, and its record id: none for an element that is not an event.
pub(crate) fn event(c: &Compiled, e: &El, now_ns: i64) -> Option<(Option<Record>, u64)> {
    if e.name != "Event" {
        return None;
    }
    let sys = e.child("System")?;
    let id = sys.text_at(&["EventRecordID"])?.parse::<u64>().ok()?;
    let provider = sys.child("Provider").and_then(|p| p.attr("Name")).filter(|p| !p.is_empty());
    let event_id = sys.text_at(&["EventID"]);
    let rendered = e.child("RenderingInfo");
    let data: Vec<(String, String)> = e
        .child("EventData")
        .or_else(|| e.child("UserData").and_then(|u| u.children.first()))
        .map(|d| d.children.iter().enumerate().filter(|(_, x)| !x.text.trim().is_empty()).map(|(i, x)| (x.attr("Name").map_or_else(|| x.name.clone().replace("Data", &i.to_string()), str::to_owned), x.text.trim().to_owned())).collect())
        .unwrap_or_default();
    let mut message = rendered.and_then(|r| r.text_at(&["Message"])).map(str::to_owned).unwrap_or_else(|| {
        // No message rendered (its provider's not installed here): what it says, plainly.
        let mut m = format!("{} event {}", provider.unwrap_or("Windows"), event_id.unwrap_or("?"));
        for (k, v) in &data {
            m.push_str(&format!(" {k}={v}"));
        }
        m
    });
    if !c.keep(&mut message) {
        return Some((None, id));
    }
    let ts = sys.child("TimeCreated").and_then(|t| t.attr("SystemTime")).and_then(system_time).filter(|&t| t <= now_ns + super::AHEAD_MS * 1_000_000).unwrap_or(now_ns);
    let mut attrs = Vec::new();
    let mut put = |k: &str, v: Option<&str>| {
        if let Some(v) = v.filter(|v| !v.is_empty()) {
            attrs.push((k.to_owned(), v.to_owned()));
        }
    };
    put("windows.channel", sys.text_at(&["Channel"]));
    put("windows.provider", provider);
    put("windows.event_id", event_id);
    put("windows.record_id", Some(&id.to_string()));
    put("windows.computer", sys.text_at(&["Computer"]));
    put("windows.task", rendered.and_then(|r| r.text_at(&["Task"])).or_else(|| sys.text_at(&["Task"])));
    put("windows.opcode", rendered.and_then(|r| r.text_at(&["Opcode"])));
    put("windows.keywords", rendered.and_then(|r| r.child("Keywords")).map(|k| k.children.iter().map(|x| x.text.trim()).collect::<Vec<_>>().join(",")).as_deref());
    put("process.pid", sys.child("Execution").and_then(|x| x.attr("ProcessID")));
    put("thread.id", sys.child("Execution").and_then(|x| x.attr("ThreadID")));
    put("user.id", sys.child("Security").and_then(|x| x.attr("UserID")));
    for (k, v) in data {
        attrs.push((format!("windows.event_data.{k}"), v));
    }
    attrs.extend(c.tags.iter().cloned());
    let service = c.service.clone().or_else(|| provider.map(str::to_owned)).unwrap_or_else(|| "windows".into());
    let r = Record { ts, severity: severity(sys.text_at(&["Level"])), service, host: c.host.clone(), source: c.source.clone(), message: crate::observe::cloud::message(message), attrs, ..Record::default() };
    Some((Some(r), id))
}

fn wevtutil() -> String {
    std::env::var("KILN_WEVTUTIL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "wevtutil".into())
}

/// The XPath that asks `channel` for the events after `after`, the source's query joined to it.
fn xpath(c: &Compiled, after: u64) -> String {
    let mine = format!("System[(EventRecordID>{after})]");
    match c.query.as_deref().map(str::trim).and_then(|q| q.strip_prefix("*[")).and_then(|q| q.strip_suffix(']')) {
        Some(inner) if !inner.trim().is_empty() => format!("*[{mine} and ({inner})]"),
        _ => format!("*[{mine}]"),
    }
}

/// `wevtutil` run with `args`, its output (30 s at the most).
fn call(args: &[String]) -> Result<String, String> {
    let (lines, status, said) = super::output_lines(Command::new(wevtutil()).args(args), "wevtutil", CALL_MAX).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound { format!("{} is not here: the Event Log is Windows'", wevtutil()) } else { format!("{}: {e}", wevtutil()) }
    })?;
    match status {
        None => Err(format!("{} took more than {} s", wevtutil(), CALL_MAX.as_secs())),
        Some(s) if !s.success() => Err(format!("{} ended ({s}): {}", wevtutil(), said.trim())),
        Some(_) => Ok(lines.concat()),
    }
}

/// The newest event's record id in `channel` (0 when it holds none).
fn newest(channel: &str) -> Result<u64, String> {
    let out = call(&["qe".into(), channel.to_owned(), "/c:1".into(), "/rd:true".into(), "/f:xml".into()])?;
    Ok(parse(&out).iter().filter_map(|e| e.child("System")?.text_at(&["EventRecordID"])?.parse::<u64>().ok()).max().unwrap_or(0))
}

/// The events of `channel` after `after` the source asks for, oldest first.
fn after(c: &Compiled, channel: &str, after: u64, n: usize) -> Result<Vec<El>, String> {
    let out = call(&["qe".into(), channel.to_owned(), format!("/q:{}", xpath(c, after)), "/f:RenderedXml".into(), format!("/c:{n}"), "/rd:false".into()])?;
    Ok(parse(&out))
}

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    /// Each channel's last event written.
    #[serde(default)]
    channels: BTreeMap<String, u64>,
}

/// The Event Log source's thread: each channel asked each second for what came after its place,
/// a place kept once its records are written.
pub(crate) fn run(c: &Compiled, saved_to: &Path, stop: &Stop, state: &Mutex<State>, offer: Offer<'_>) {
    let mut places: BTreeMap<String, u64> = std::fs::read(saved_to).ok().and_then(|b| serde_json::from_slice::<Saved>(&b).ok()).map(|s| s.channels).unwrap_or_default();
    // Each mark: a channel and its place once its batch is written.
    let mut batch: Batch<(String, u64)> = Batch::default();
    let mut flight: InFlight<(String, u64)> = InFlight::default();
    // Where each channel was read to (its batch maybe not written yet).
    let mut read_to: BTreeMap<String, u64> = BTreeMap::new();
    let (mut records, mut excluded, mut errors, mut bytes) = (0u64, 0u64, 0u64, 0u64);
    let mut last_ms = None;
    let mut error: Option<String> = None;
    let mut saved = Instant::now();
    let mut dirty = false;
    let save = |places: &BTreeMap<String, u64>| -> Result<(), String> {
        serde_json::to_vec(&Saved { channels: places.clone() }).map_err(std::io::Error::other).and_then(|b| crate::sync::group::save_atomic(saved_to, &b)).map_err(|e| format!("its places could not be written down: {e}"))
    };
    while !stop.stopped() {
        let mut round_error = None;
        for channel in &c.channels {
            if stop.stopped() {
                break;
            }
            // An offer refused before goes first.
            if !batch.is_empty() && !flight.offer(&mut batch, offer) {
                break;
            }
            let place = match read_to.get(channel).or_else(|| places.get(channel)) {
                Some(&p) => p,
                None if c.start == Start::Beginning => 0,
                None => match newest(channel) {
                    Ok(n) => {
                        read_to.insert(channel.clone(), n);
                        continue;
                    }
                    Err(e) => {
                        round_error = Some(format!("{channel}: {e}"));
                        continue;
                    }
                },
            };
            let now_ns = crate::observe::clock_ms().saturating_mul(1_000_000);
            match after(c, channel, place, BATCH) {
                Ok(events) if events.is_empty() => {
                    // Cleared? Its newest below the place: read from its start.
                    if place > 0 && newest(channel).is_ok_and(|n| n < place) {
                        tracing::info!(source = %c.name, %channel, place, "an event channel was cleared: read from its start");
                        read_to.insert(channel.clone(), 0);
                    }
                }
                Ok(events) => {
                    let mut to = place;
                    for e in &events {
                        let Some((r, id)) = event(c, e, now_ns) else { continue };
                        bytes += r.as_ref().map_or(0, |r| r.message.len() as u64);
                        match r {
                            Some(r) => batch.push(r),
                            None => excluded += 1,
                        }
                        to = to.max(id);
                    }
                    read_to.insert(channel.clone(), to);
                    batch.marks.retain(|(ch, _)| ch != channel);
                    batch.marks.push((channel.clone(), to));
                    let _ = flight.offer(&mut batch, offer);
                }
                Err(e) => round_error = Some(format!("{channel}: {e}")),
            }
        }
        if let Some(e) = round_error {
            errors += 1;
            error = Some(e);
        }
        let before = records;
        flight.written(|marks, n| {
            for (ch, to) in marks {
                places.insert(ch, to);
                dirty = true;
            }
            records += n;
        });
        if records > before {
            last_ms = Some(crate::observe::clock_ms());
            error = None;
        }
        if dirty && saved.elapsed() >= SAVE_EVERY {
            if let Err(e) = save(&places) {
                error = Some(e);
            }
            dirty = false;
            saved = Instant::now();
        }
        report(state, &error, errors, records, flight.records(), excluded, bytes, last_ms);
        stop.sleep(POLL);
    }
    flight.drain(|marks, n| {
        for (ch, to) in marks {
            places.insert(ch, to);
        }
        records += n;
    });
    if let Err(e) = save(&places) {
        error = Some(e);
    }
    report(state, &error, errors, records, flight.records(), excluded, bytes, last_ms);
    tracing::info!(source = %c.name, places = ?places, not_written = flight.records(), "log source stopped: its places written down");
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

/// Each channel's last `n` events `c` reads, nothing written.
pub(crate) fn sample(c: &Compiled, n: usize) -> super::files::Sampled {
    let mut out = super::files::Sampled { files: Vec::new(), matched: 0, records: Vec::new(), excluded: 0, error: None };
    let now_ns = crate::observe::clock_ms().saturating_mul(1_000_000);
    for channel in &c.channels {
        let from = match newest(channel) {
            Ok(top) => top.saturating_sub(n as u64),
            Err(e) => {
                out.error = Some(format!("{channel}: {e}"));
                continue;
            }
        };
        match after(c, channel, from, n) {
            Ok(events) => {
                for e in &events {
                    match event(c, e, now_ns) {
                        Some((Some(r), _)) => out.records.push(r),
                        Some((None, _)) => out.excluded += 1,
                        None => {}
                    }
                }
            }
            Err(e) => out.error = Some(format!("{channel}: {e}")),
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::observe::collect::{Kind, Source};

    /// An event as `wevtutil qe System /f:RenderedXml` writes it.
    const EVENT: &str = r#"<Event xmlns='http://schemas.microsoft.com/win/2004/08/events/event'><System><Provider Name='Service Control Manager' Guid='{555908d1-a6d7-4695-8e1e-26931d2012f4}' EventSourceName='Service Control Manager'/><EventID Qualifiers='49152'>7031</EventID><Version>0</Version><Level>2</Level><Task>0</Task><Opcode>0</Opcode><Keywords>0x8080000000000000</Keywords><TimeCreated SystemTime='2026-10-19T11:01:27.1771960Z'/><EventRecordID>48213</EventRecordID><Correlation/><Execution ProcessID='764' ThreadID='9192'/><Channel>System</Channel><Computer>build-07.corp.example</Computer><Security/></System><EventData><Data Name='param1'>Print Spooler</Data><Data Name='param2'>1</Data><Data Name='param3'>60000</Data><Data Name='param4'>1</Data><Data Name='param5'>Restart the service</Data><Data Name='param6'>Spooler</Data></EventData><RenderingInfo Culture='en-US'><Message>The Print Spooler service terminated unexpectedly.  It has done this 1 time(s).  The following corrective action will be taken in 60000 milliseconds: Restart the service.</Message><Level>Error</Level><Task></Task><Opcode></Opcode><Channel>System</Channel><Provider>Microsoft-Windows-Service Control Manager</Provider><Keywords><Keyword>Classic</Keyword></Keywords></RenderingInfo></Event>"#;

    fn source() -> Source {
        serde_json::from_value(serde_json::json!({ "type": "windows_event", "channels": ["System", "Application"], "query": "*[System[(Level=1 or Level=2)]]", "tags": ["team:win"] })).unwrap()
    }

    #[test]
    fn an_event_reads_as_a_record_with_its_record_id() {
        let c = source().compile("win", "build-07").unwrap();
        assert_eq!(c.kind, Kind::WindowsEvent);
        let els = parse(&format!("{EVENT}\r\n{}", EVENT.replace("48213", "48214")));
        assert_eq!(els.len(), 2);
        let (r, id) = event(&c, &els[0], i64::MAX / 2).unwrap();
        let r = r.unwrap();
        assert_eq!(id, 48213);
        assert_eq!(r.ts, 1_792_407_687_177_196_000);
        assert_eq!((r.severity, r.service.as_str(), r.source.as_str(), r.host.as_str()), (17, "Service Control Manager", "windows_event", "build-07"));
        assert!(r.message.starts_with("The Print Spooler service terminated unexpectedly."), "{}", r.message);
        let attr = |k: &str| r.attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str());
        for (k, v) in [("windows.channel", "System"), ("windows.event_id", "7031"), ("windows.record_id", "48213"), ("windows.computer", "build-07.corp.example"), ("windows.keywords", "Classic"), ("process.pid", "764"), ("windows.event_data.param1", "Print Spooler"), ("windows.event_data.param6", "Spooler"), ("team", "win")] {
            assert_eq!(attr(k), Some(v), "{k}: {:?}", r.attrs);
        }
        // No message rendered (its provider not installed): what it says, plainly.
        let bare = parse(&EVENT.replace(&EVENT[EVENT.find("<RenderingInfo").unwrap()..EVENT.find("</Event>").unwrap()], ""));
        let (r, _) = event(&c, &bare[0], i64::MAX / 2).unwrap();
        assert!(r.unwrap().message.starts_with("Service Control Manager event 7031 param1=Print Spooler"));
    }

    #[test]
    fn xml_reads_escapes_quotes_and_what_is_not_an_element() {
        let els = parse("<?xml version='1.0'?><!-- c --><a x=\"1 &amp; 2\" y='&lt;b&gt;'>t &#65;&#x42; <b/><c>in<![CDATA[ <raw> ]]></c></a><d/>");
        assert_eq!(els.len(), 2);
        assert_eq!((els[0].attr("x"), els[0].attr("y")), (Some("1 & 2"), Some("<b>")));
        assert_eq!(els[0].text.trim(), "t AB");
        assert_eq!(els[0].children.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["b", "c"]);
        assert_eq!(els[0].text_at(&["c"]), Some("in <raw>"));
        // A `>` inside a quoted value is the value's.
        assert_eq!(parse("<a v='x>y'/>")[0].attr("v"), Some("x>y"));
    }

    #[test]
    fn a_source_s_query_is_joined_to_its_place() {
        let c = source().compile("win", "h").unwrap();
        assert_eq!(xpath(&c, 41), "*[System[(EventRecordID>41)] and (System[(Level=1 or Level=2)])]");
        let plain: Source = serde_json::from_value(serde_json::json!({ "type": "windows_event", "channels": ["System"] })).unwrap();
        assert_eq!(xpath(&plain.compile("w", "h").unwrap(), 0), "*[System[(EventRecordID>0)]]");
        for bad in [
            serde_json::json!({ "type": "windows_event" }),
            serde_json::json!({ "type": "windows_event", "channels": ["System"], "query": "Level=1" }),
            serde_json::json!({ "type": "windows_event", "channels": ["System"], "paths": ["C:\\x"] }),
            serde_json::json!({ "type": "file", "paths": ["/x"], "channels": ["System"] }),
        ] {
            let s: Source = serde_json::from_value(bad.clone()).unwrap();
            assert!(s.check().is_err(), "{bad}");
        }
    }
}
