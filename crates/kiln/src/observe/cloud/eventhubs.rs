//! Azure Event Hubs (`docs/observe.md` §10.3): what Azure Monitor's diagnostic settings stream to
//! a hub (resources' logs, the activity log, Entra ID's logs), read as it comes over Event Hubs'
//! Kafka endpoint (the Standard tier and above; [`crate::observe::kafka`]), on the server.
//!
//! - A hub is a topic: each of its partitions read from where kiln last read it, else from the
//!   integration's lookback ago (the first event since, by `ListOffsets`' time), else from its end.
//!   An offset is kept only once what came before it is written (its write applied), and written
//!   down each second: a stop repeats nothing, a kill the last second at most. An offset retention
//!   has passed is read from the earliest still there, and said.
//! - Credentials: a connection string with Listen rights (SAS: SASL PLAIN, its user
//!   `$ConnectionString`), else the integration's Azure credentials (Microsoft Entra ID:
//!   OAUTHBEARER, a token for the namespace; the role Azure Event Hubs Data Receiver).
//! - An event's body is `{"records": [...]}` (or one record, or text): each record is a log record
//!   as Datadog's Azure forwarder makes it: `source` `azure.<provider>` (`Microsoft.Web`:
//!   `azure.web`), tags `subscription_id`, `resource_group` and `tenant` from its `resourceId`;
//!   `service` the resource's name, as the Log Analytics pull has it (§10.1); its time `time`; its
//!   level `level` (`Informational`, `Warning`, `Error`, `Critical`), else a failed `resultType`
//!   an error; its message `properties.message`, `message`, `resultDescription` or
//!   `operationName`, else the record; its other values attributes. Metric records (a diagnostic
//!   setting's `AllMetrics`) are counted and not read: Azure Monitor's metrics are pulled.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kiln_obs::logs::record::Record;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Api, Status, Stream, id_of, parse_time_ns};
use crate::observe::collect::{Batch, InFlight};
use crate::observe::kafka::{Consumer, EARLIEST, LATEST, Sasl, Source};

/// Event Hubs' Kafka endpoint's port.
const PORT: u16 = 9093;
/// Hubs an integration reads at the most.
const HUBS: usize = 32;
/// A record's attributes from its values, at the most (as a Log Analytics row's).
const ATTRS: usize = 32;
/// How often offsets kept are written down, and kiln's own series of a stream.
const SAVE_EVERY: Duration = Duration::from_secs(1);
const TALLY_EVERY: Duration = Duration::from_secs(15);
/// How long a stopping stream waits for its last writes before it writes its offsets down.
const STOP_WAIT: Duration = Duration::from_secs(2);
/// A failed stream is tried again after 1 s, doubling, up to a minute.
const RETRY_FIRST: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(60);

fn yes() -> bool {
    true
}

fn is_true(b: &bool) -> bool {
    *b
}

/// An Azure integration's event hubs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// The namespace: its name (`kiln-logs`) or its host (`kiln-logs.servicebus.windows.net`).
    pub namespace: String,
    /// The hubs read, each a topic.
    pub hubs: Vec<String>,
    /// A connection string of the namespace (or a hub) with Listen rights: SAS. None: the
    /// integration's Azure credentials (Entra ID).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_string: Option<String>,
    /// The Kafka endpoint (`host:port`) instead of the namespace's: a national cloud's, a test's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// TLS to it, as Event Hubs requires; off only with an `endpoint` of its own (a test's broker).
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub tls: bool,
}

impl Config {
    pub fn check(&self) -> Result<(), String> {
        let host = self.host();
        if self.namespace.is_empty() || host.len() > 253 || !host.bytes().all(|b| b.is_ascii_alphanumeric() || b"-.".contains(&b)) {
            return Err(format!("event hubs' namespace {:?}: its name or its host", self.namespace));
        }
        if self.hubs.is_empty() || self.hubs.len() > HUBS {
            return Err(format!("event hubs: 1 to {HUBS} hubs"));
        }
        // Event Hubs' names: letters, digits, '.', '-', '_', 256 at the most; a Kafka topic's: 249.
        if let Some(h) = self.hubs.iter().find(|h| h.is_empty() || h.len() > 249 || !h.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))) {
            return Err(format!("event hub {h:?}: letters, digits, '.', '-' and '_', 249 at the most"));
        }
        if let Some(cs) = &self.connection_string
            && !cs.starts_with("Endpoint=sb://")
        {
            return Err("an event hubs connection string starts Endpoint=sb://".into());
        }
        if let Some(e) = &self.endpoint
            && e.rsplit_once(':').is_none_or(|(h, p)| h.is_empty() || p.parse::<u16>().is_err())
        {
            return Err(format!("event hubs' endpoint {e:?}: host:port"));
        }
        if !self.tls && self.endpoint.is_none() {
            return Err("event hubs without TLS: only to an endpoint of its own".into());
        }
        Ok(())
    }

    /// The namespace's host.
    pub fn host(&self) -> String {
        if self.namespace.contains('.') { self.namespace.clone() } else { format!("{}.servicebus.windows.net", self.namespace) }
    }

    /// Where its Kafka endpoint is.
    fn bootstrap(&self) -> (String, u16) {
        match self.endpoint.as_deref().and_then(|e| e.rsplit_once(':')) {
            Some((h, p)) => (h.trim_matches(['[', ']']).to_owned(), p.parse().unwrap_or(PORT)),
            None => (self.host(), PORT),
        }
    }
}

// --------------------------------------------------------------------------------------- records

/// A record's value by the first of `names` it has (not null, not empty).
fn first<'a>(o: &'a serde_json::Map<String, Value>, names: &[&str]) -> Option<&'a Value> {
    names.iter().find_map(|k| o.get(*k)).filter(|v| !v.is_null() && v.as_str() != Some(""))
}

fn text(v: &Value) -> String {
    v.as_str().map_or_else(|| v.to_string(), str::to_owned)
}

/// Where an event is: its hub's namespace and name, its partition and offset.
#[derive(Debug, Clone, Copy)]
pub struct Place<'a> {
    pub namespace: &'a str,
    pub hub: &'a str,
    pub partition: i32,
    pub offset: i64,
}

/// One event's records (its body, `value`), each with its id: a diagnostic setting's
/// `{"records": [...]}`, one record, or else its text as one. `at_ms` is the event's own time (its
/// record's in Kafka), for records without one. Metric records are counted, not read.
pub fn records_of(value: &[u8], at_ms: i64, place: Place<'_>, tags: &[(String, String)], out: &mut Vec<(u64, Record)>) -> u64 {
    let id = |i: usize| id_of(&["eventhubs", place.namespace, place.hub, &place.partition.to_string(), &place.offset.to_string(), &i.to_string()]);
    let parsed: Option<Value> = serde_json::from_slice(value).ok();
    let each: Vec<&serde_json::Map<String, Value>> = match &parsed {
        Some(Value::Object(o)) => match o.get("records") {
            Some(Value::Array(rs)) => rs.iter().filter_map(Value::as_object).collect(),
            _ => vec![o],
        },
        Some(Value::Array(rs)) => rs.iter().filter_map(Value::as_object).collect(),
        _ => Vec::new(),
    };
    if each.is_empty() {
        // Not JSON (or JSON of no record): the event's text, a record of its own.
        let message = String::from_utf8_lossy(value).into_owned();
        if message.trim().is_empty() {
            return 0;
        }
        let mut attrs = vec![("azure.event_hub".to_owned(), place.hub.to_owned())];
        attrs.extend(tags.iter().cloned());
        let severity = super::severity_of(&message);
        out.push((id(0), Record { ts: at_ms.saturating_mul(1_000_000), severity, service: place.hub.to_owned(), host: String::new(), source: "azure".into(), message: super::message(message), attrs, trace_id: None, span_id: None }));
        return 0;
    }
    let mut skipped = 0;
    for (i, o) in each.into_iter().enumerate() {
        // A metric record (`AllMetrics`): Azure Monitor's metrics are pulled.
        if o.contains_key("metricName") && ["average", "total", "count", "minimum", "maximum"].iter().any(|k| o.contains_key(*k)) {
            skipped += 1;
            continue;
        }
        out.push((id(i), record_of(o, at_ms, place.hub, tags)));
    }
    skipped
}

/// A diagnostic record as a log record (the module's doc says how).
fn record_of(o: &serde_json::Map<String, Value>, at_ms: i64, hub: &str, tags: &[(String, String)]) -> Record {
    let ts = first(o, &["time", "Time", "timeStamp", "timestamp", "TimeGenerated", "eventTimestamp"]).and_then(Value::as_str).and_then(parse_time_ns).unwrap_or(at_ms.saturating_mul(1_000_000));
    let resource = first(o, &["resourceId", "ResourceId", "resourceID", "_ResourceId"]).map(text).unwrap_or_default();
    let provider = super::azure::segment(&resource, "providers").map(str::to_ascii_lowercase);
    let source = provider.as_deref().map_or_else(|| "azure".to_owned(), |p| format!("azure.{}", p.strip_prefix("microsoft.").unwrap_or(p)));
    let service = resource.rsplit('/').find(|p| !p.is_empty()).unwrap_or(hub).to_owned();
    let host = first(o, &["Host", "host", "hostname", "hostName", "Computer"]).map(text).unwrap_or_default();
    let properties = o.get("properties").and_then(Value::as_object);
    let said = properties.and_then(|p| first(p, &["message", "Message", "msg"])).or_else(|| first(o, &["message", "Message", "resultDescription", "ResultDescription", "operationName", "OperationName"]));
    let message = said.map_or_else(|| Value::Object(o.clone()).to_string(), text);
    let level = first(o, &["level", "Level", "severity", "Severity", "severityLevel"]).map(text);
    let failed = first(o, &["resultType", "ResultType"]).and_then(Value::as_str).is_some_and(|r| r.eq_ignore_ascii_case("failure") || r.eq_ignore_ascii_case("failed"));
    let severity = match level.as_deref().and_then(kiln_obs::logs::record::severities) {
        Some(r) => *r.start(),
        None if failed => 17,
        None => super::severity_of(&message),
    };
    let mut attrs: Vec<(String, String)> = Vec::with_capacity(8 + tags.len());
    for (k, name) in [("subscription_id", "subscriptions"), ("resource_group", "resourcegroups"), ("tenant", "tenants")] {
        if let Some(v) = super::azure::segment(&resource, name).filter(|v| !v.is_empty()) {
            attrs.push((k.to_owned(), v.to_owned()));
        }
    }
    if !resource.is_empty() {
        attrs.push(("azure.resource_id".into(), resource.clone()));
    }
    attrs.push(("azure.event_hub".into(), hub.to_owned()));
    // What a record's own fields are, and a message (it is the message): not attributes again.
    let skip = |k: &str| {
        matches!(k, "time" | "Time" | "timeStamp" | "timestamp" | "TimeGenerated" | "eventTimestamp" | "resourceId" | "ResourceId" | "resourceID" | "_ResourceId" | "properties" | "Host" | "host" | "hostname" | "hostName" | "Computer" | "level" | "Level" | "message" | "Message")
    };
    let mut room = ATTRS;
    for (k, v) in o.iter().filter(|(k, v)| !skip(k) && !v.is_null() && !v.is_object() && !v.is_array() && v.as_str() != Some("")) {
        if room == 0 {
            break;
        }
        attrs.push((k.clone(), text(v)));
        room -= 1;
    }
    for (k, v) in properties.into_iter().flatten().filter(|(k, v)| !matches!(k.as_str(), "message" | "Message" | "msg") && !v.is_null() && !v.is_object() && !v.is_array() && v.as_str() != Some("")) {
        if room == 0 {
            break;
        }
        attrs.push((format!("properties.{k}"), text(v)));
        room -= 1;
    }
    for (k, v) in tags {
        if !attrs.iter().any(|(a, _)| a == k) {
            attrs.push((k.clone(), v.clone()));
        }
    }
    Record { ts, severity, service, host, source, message: super::message(message), attrs, trace_id: None, span_id: None }
}

// --------------------------------------------------------------------------------------- offsets

/// The offsets kept of an integration's hubs: each hub's partitions' next offset to read.
pub type Offsets = BTreeMap<String, BTreeMap<i32, i64>>;

fn load(path: &std::path::Path) -> Offsets {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// An integration's offsets, kept from one of its runs to the next (its settings changed) and in
/// its file: each hub's stream writes its own hub's, one stream a hub at a time.
pub struct Kept {
    path: PathBuf,
    all: Mutex<Offsets>,
    /// Hubs a stream reads now.
    reading: Mutex<std::collections::HashSet<String>>,
    /// The integration was deleted: nothing more is written down.
    gone: std::sync::atomic::AtomicBool,
}

/// A hub's turn to be read, given back when its stream ends.
struct Turn<'a> {
    kept: &'a Kept,
    hub: String,
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        super::lock(&self.kept.reading).remove(&self.hub);
    }
}

impl Kept {
    pub fn open(path: PathBuf) -> Arc<Self> {
        let all = load(&path);
        Arc::new(Self { path, all: Mutex::new(all), reading: Mutex::default(), gone: std::sync::atomic::AtomicBool::new(false) })
    }

    /// Waits for the stream of `hub` before this one (its settings were changed) to have written
    /// its offsets down and ended.
    async fn turn(&self, hub: &str) -> Turn<'_> {
        while !super::lock(&self.reading).insert(hub.to_owned()) {
            crate::rt::sleep(Duration::from_millis(50)).await;
        }
        Turn { kept: self, hub: hub.to_owned() }
    }

    fn of(&self, hub: &str) -> BTreeMap<i32, i64> {
        super::lock(&self.all).get(hub).cloned().unwrap_or_default()
    }

    fn save(&self, hub: &str, offsets: &BTreeMap<i32, i64>) -> anyhow::Result<()> {
        let mut all = super::lock(&self.all);
        all.insert(hub.to_owned(), offsets.clone());
        if self.gone.load(std::sync::atomic::Ordering::Acquire) {
            return Ok(());
        }
        crate::util::write_secret(&self.path, &serde_json::to_vec(&*all)?, 0o600)?;
        Ok(())
    }

    /// The integration is deleted: its file goes, and nothing more is written to it.
    pub fn forget(&self) {
        self.gone.store(true, std::sync::atomic::Ordering::Release);
        let _all = super::lock(&self.all);
        let _ = std::fs::remove_file(&self.path);
    }
}

// ---------------------------------------------------------------------------------------- stream

/// What a stream needs: the integration's name, its Azure settings (for Entra's tokens), the hub,
/// where offsets are kept, and its status to say what it does.
pub struct Reader {
    pub name: String,
    pub azure: super::azure::Config,
    pub hub: String,
    pub kept: Arc<Kept>,
    pub http: Arc<crate::http::client::Client>,
    pub status: Arc<Mutex<Status>>,
}

/// The connection's SASL: the connection string's, else an Entra token for the namespace.
async fn sasl(r: &Reader, hubs: &Config) -> anyhow::Result<Sasl> {
    if let Some(cs) = &hubs.connection_string {
        return Ok(Sasl::Plain { user: "$ConnectionString".into(), password: cs.clone() });
    }
    let api = Api::new(&r.http);
    let token = super::azure::token(&api, &r.azure, &format!("https://{}", hubs.host()), super::super::clock_ms()).await?;
    Ok(Sasl::OAuthBearer { token: token.value })
}

fn update(r: &Reader, f: impl FnOnce(&mut Stream)) {
    let mut s = super::lock(&r.status);
    if let Some(st) = s.streams.iter_mut().find(|x| x.hub == r.hub) {
        f(st);
    } else {
        let mut st = Stream { hub: r.hub.clone(), state: "starting", ..Stream::default() };
        f(&mut st);
        s.streams.push(st);
    }
}

/// What a stream has offered and what of it is written, kept from one connection to the next.
struct Progress {
    /// Each partition's offset after what is written (the writes applied).
    written: BTreeMap<i32, i64>,
    /// What is written down.
    saved: BTreeMap<i32, i64>,
    inflight: InFlight<(i32, i64)>,
    last_save: Instant,
}

impl Progress {
    /// Moves the offsets past what has been written since; the records it came to.
    fn advance(&mut self) -> u64 {
        let (written, mut n) = (&mut self.written, 0);
        self.inflight.written(|marks, records| {
            n += records;
            for (p, o) in marks {
                written.insert(p, o);
            }
        });
        n
    }

    fn save(&mut self, r: &Reader) -> anyhow::Result<()> {
        if self.written != self.saved {
            r.kept.save(&r.hub, &self.written)?;
            self.saved = self.written.clone();
        }
        self.last_save = Instant::now();
        Ok(())
    }

    /// Waits a moment for what was offered to be written ([`STOP_WAIT`]), then writes the offsets
    /// down: a connection that failed, or a stream stopping, repeats only what was not written.
    async fn settle(&mut self, r: &Reader) {
        let end = Instant::now() + STOP_WAIT;
        loop {
            let n = self.advance();
            if n > 0 {
                update(r, |s| {
                    s.records += n;
                    s.last_ms = Some(super::super::clock_ms());
                });
            }
            if self.inflight.queue.is_empty() || Instant::now() >= end {
                break;
            }
            crate::rt::sleep(Duration::from_millis(20)).await;
        }
        // What is still in flight is read again: forgotten here, its records offered again.
        self.inflight.queue.clear();
        if let Err(e) = self.save(r) {
            tracing::warn!(integration = %r.name, hub = %r.hub, error = %format!("{e:#}"), "an event hub's offsets were not written down");
        }
    }
}

/// Reads the hub until `stop` or `server` is cancelled: connected again after a failure, 1 s then
/// doubling up to a minute, each failure and recovery an event once.
pub async fn run(r: Reader, stop: crate::rt::Shutdown, server: crate::rt::Shutdown) {
    let Some(hubs) = r.azure.event_hubs.clone() else { return };
    let mut failed = 0u32;
    update(&r, |s| s.state = "starting");
    let kept = r.kept.clone();
    let _turn = kept.turn(&r.hub).await;
    let written = kept.of(&r.hub);
    let mut progress = Progress { saved: written.clone(), written, inflight: InFlight::default(), last_save: Instant::now() };
    loop {
        if stop.is_cancelled() || server.is_cancelled() {
            return;
        }
        let outcome = read(&r, &hubs, &stop, &server, &mut failed, &mut progress).await;
        progress.settle(&r).await;
        if stop.is_cancelled() || server.is_cancelled() {
            return;
        }
        let why = match outcome {
            Ok(()) => return,
            Err(e) => format!("{e:#}"),
        };
        failed += 1;
        update(&r, |s| {
            s.state = "failing";
            s.error = Some(why.clone());
        });
        if failed == 1 {
            tracing::warn!(integration = %r.name, hub = %r.hub, error = %why, "an event hub's stream failed");
            event(&r, 17, "failing", format!("azure integration {}'s event hub {} is failing: {why}", r.name, r.hub));
        }
        let wait = RETRY_FIRST.saturating_mul(1 << (failed - 1).min(6)).min(RETRY_MAX);
        if !stop.sleep(wait).await {
            return;
        }
    }
}

/// One connection's reading, until it fails (an error) or is stopped (`Ok`): from the offsets
/// written, each partition without one from the lookback.
async fn read(r: &Reader, hubs: &Config, stop: &crate::rt::Shutdown, server: &crate::rt::Shutdown, failed: &mut u32, progress: &mut Progress) -> anyhow::Result<()> {
    let tls = if hubs.tls { Some(Arc::new(crate::http::tls::ClientTls::new(crate::http::client::Roots::WebPki, None)?)) } else { None };
    let source = Source { bootstrap: vec![hubs.bootstrap()], topic: r.hub.clone(), tls, sasl: Some(sasl(r, hubs).await?), client: format!("kiln-{}", r.name) };
    let mut consumer = Consumer::new(source);
    let partitions = consumer.refresh().await?;
    let mut next = progress.written.clone();
    next.retain(|p, _| partitions.contains(p));
    let fresh: Vec<i32> = partitions.iter().copied().filter(|p| !next.contains_key(p)).collect();
    if !fresh.is_empty() {
        let since = super::super::clock_ms().saturating_sub(r.azure.common.lookback_ms());
        let found = consumer.offsets_at(&fresh, since).await?;
        let none: Vec<i32> = fresh.iter().copied().filter(|p| found.get(p).is_none_or(|o| *o < 0)).collect();
        let ends = if none.is_empty() { BTreeMap::new() } else { consumer.offsets_at(&none, LATEST).await? };
        for p in fresh {
            let o = found.get(&p).copied().filter(|o| *o >= 0).or_else(|| ends.get(&p).copied()).unwrap_or(0);
            next.insert(p, o);
            progress.written.insert(p, o);
        }
    }
    let tags = r.azure.common.tag_pairs();
    let namespace = hubs.host();
    let mut pending: VecDeque<Batch<(i32, i64)>> = VecDeque::new();
    let mut last_tally = Instant::now();
    let (mut tally_records, mut recovered) = (0u64, false);
    let offer = |logs: crate::observe::ingest::Logs, ack: crate::observe::ingest::Ack| match super::super::api::get() {
        Some(o) => o.offer_log_batch(logs, ack),
        None => Err(logs),
    };
    update(r, |s| {
        s.partitions = partitions.len();
        s.state = "reading";
        s.error = None;
    });
    loop {
        // What has been written moves the offsets kept, written down each second.
        let n = progress.advance();
        tally_records += n;
        if n > 0 {
            update(r, |s| {
                s.records += n;
                s.last_ms = Some(super::super::clock_ms());
            });
        }
        if progress.last_save.elapsed() >= SAVE_EVERY {
            progress.save(r)?;
        }
        if last_tally.elapsed() >= TALLY_EVERY {
            let lag = super::lock(&r.status).streams.iter().find(|s| s.hub == r.hub).map_or(0, |s| s.lag);
            tally(&r.name, &r.hub, tally_records, lag);
            tally_records = 0;
            last_tally = Instant::now();
        }
        if stop.is_cancelled() || server.is_cancelled() {
            return Ok(());
        }
        // Batches read and not yet taken go first; observe busy, they wait and nothing more is read.
        if let Some(b) = pending.front_mut() {
            if progress.inflight.offer(b, &offer) {
                pending.pop_front();
            } else {
                crate::rt::sleep(Duration::from_millis(200)).await;
            }
            continue;
        }
        let mut batches: Vec<Batch<(i32, i64)>> = vec![Batch::default()];
        let mut skipped = 0u64;
        let mut events = 0u64;
        let mut records: Vec<(u64, Record)> = Vec::new();
        let round = consumer
            .fetch(&next, &mut |p, rec| {
                events += 1;
                records.clear();
                skipped += records_of(rec.value.unwrap_or_default(), rec.ts_ms, Place { namespace: &namespace, hub: &r.hub, partition: p, offset: rec.offset }, &tags, &mut records);
                let b = match batches.last_mut() {
                    Some(b) if !b.full() => b,
                    _ => {
                        batches.push(Batch::default());
                        let Some(b) = batches.last_mut() else { return };
                        b
                    }
                };
                for (_, rec) in records.drain(..) {
                    b.push(rec);
                }
                // This event's records are in: the partition's offset moves past it once they are written.
                match b.marks.iter_mut().find(|(q, _)| *q == p) {
                    Some(m) => m.1 = rec.offset + 1,
                    None => b.marks.push((p, rec.offset + 1)),
                }
            })
            .await?;
        // Partitions whose offsets moved without records (markers, gaps): moved when all is written.
        if let Some(last) = batches.last_mut() {
            for (&p, &o) in &round.next {
                if next.get(&p) != Some(&o) {
                    match last.marks.iter_mut().find(|(q, _)| *q == p) {
                        Some(m) => m.1 = m.1.max(o),
                        None => last.marks.push((p, o)),
                    }
                }
            }
        }
        pending.extend(batches.into_iter().filter(|b| !b.is_empty()));
        for (p, o) in &round.next {
            next.insert(*p, *o);
        }
        let lag: i64 = round.lag.values().sum();
        update(r, |s| {
            s.lag = lag;
            s.events += events;
            s.skipped += skipped;
            if s.state != "reading" {
                s.state = "reading";
                s.error = None;
            }
        });
        if *failed > 0 && !recovered {
            recovered = true;
            *failed = 0;
            event(r, 9, "recovered", format!("azure integration {}'s event hub {} recovered", r.name, r.hub));
        }
        if !round.out_of_range.is_empty() {
            let earliest = consumer.offsets_at(&round.out_of_range, EARLIEST).await?;
            let latest = consumer.offsets_at(&round.out_of_range, LATEST).await?;
            for p in &round.out_of_range {
                let at = next.get(p).copied().unwrap_or(0);
                let (lo, hi) = (earliest.get(p).copied().unwrap_or(0), latest.get(p).copied().unwrap_or(0));
                let to = if at < lo { lo } else { hi };
                let said = if at < lo { format!("partition {p}: {} events passed the hub's retention before they were read", lo - at) } else { format!("partition {p}: offset {at} past its end ({hi}): read from its end") };
                tracing::warn!(integration = %r.name, hub = %r.hub, "{said}");
                update(r, |s| {
                    s.warnings.push(said.clone());
                    let n = s.warnings.len();
                    s.warnings.drain(..n.saturating_sub(10));
                });
                next.insert(*p, to);
                progress.written.insert(*p, to);
            }
        }
        if !round.moved.is_empty() {
            crate::rt::sleep(Duration::from_millis(200)).await;
            consumer.refresh().await?;
        }
        if round.throttled_ms > 0 {
            crate::rt::sleep(Duration::from_millis(u64::try_from(round.throttled_ms).unwrap_or(0).min(30_000))).await;
        }
    }
}

/// A stream's event (`source:kiln.event @event.type:cloud.stream`): its failing, its recovery.
fn event(r: &Reader, severity: u8, result: &str, message: String) {
    let Some(o) = super::super::api::get() else { return };
    let attrs = [("event.type", "cloud.stream"), ("integration", r.name.as_str()), ("cloud", "azure"), ("hub", r.hub.as_str()), ("result", result)];
    o.log(vec![Record {
        ts: super::super::clock_ms().saturating_mul(1_000_000),
        severity,
        service: "kiln.cloud".into(),
        host: String::new(),
        source: "kiln.event".into(),
        message,
        attrs: attrs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect(),
        trace_id: None,
        span_id: None,
    }]);
}

/// kiln's own series of a stream: records written, and what is left to read.
fn tally(name: &str, hub: &str, records: u64, lag: i64) {
    let Some(o) = super::super::api::get() else { return };
    let ts = super::super::clock_ms();
    let tags = [("integration", name), ("cloud", "azure"), ("hub", hub)];
    let out: Vec<_> = [("kiln.cloud.stream.records", kiln_obs::batch::Kind::Count, records as f64), ("kiln.cloud.stream.lag", kiln_obs::batch::Kind::Gauge, lag as f64)]
        .into_iter()
        .filter_map(|(m, kind, v)| Some((kiln_obs::batch::SeriesKey::new(m, tags, kind).ok()?, vec![(ts, v)])))
        .collect();
    o.write(out);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(offset: i64) -> Place<'static> {
        Place { namespace: "ns.servicebus.windows.net", hub: "insights-logs", partition: 1, offset }
    }

    /// A diagnostic setting's event: resource logs, an activity log record, a metric record, a sign-in;
    /// each record as Datadog's forwarder makes it.
    #[test]
    fn a_diagnostic_event_s_records_are_datadog_s() -> anyhow::Result<()> {
        let event = serde_json::json!({ "records": [
            { "time": "2026-10-19T06:00:00.1234567Z", "resourceId": "/SUBSCRIPTIONS/S-1/RESOURCEGROUPS/RG-1/PROVIDERS/MICROSOFT.WEB/SITES/CHECKOUT", "category": "AppServiceHTTPLogs", "operationName": "Microsoft.Web/sites/log", "level": "Informational", "properties": { "CsMethod": "GET", "ScStatus": 200, "message": "GET /cart 200" } },
            { "time": "2026-10-19T06:00:01Z", "resourceId": "/subscriptions/s-1/resourceGroups/rg-1/providers/Microsoft.Compute/virtualMachines/vm-1", "category": "Administrative", "operationName": "MICROSOFT.COMPUTE/VIRTUALMACHINES/DELETE", "resultType": "Failure", "callerIpAddress": "10.0.0.1" },
            { "time": "2026-10-19T06:00:00Z", "resourceId": "/subscriptions/s-1/resourceGroups/rg-1/providers/Microsoft.Sql/servers/db", "metricName": "cpu_percent", "average": 3.5, "count": 1, "timeGrain": "PT1M" },
            { "time": "2026-10-19T06:00:02Z", "resourceId": "/tenants/t-1/providers/Microsoft.aadiam", "category": "SignInLogs", "operationName": "Sign-in activity", "level": "4", "properties": { "userPrincipalName": "a@b.c" } }
        ] });
        let tags = [("team".to_owned(), "pay".to_owned())];
        let mut out = Vec::new();
        let skipped = records_of(event.to_string().as_bytes(), 1, place(7), &tags, &mut out);
        anyhow::ensure!(skipped == 1 && out.len() == 3, "{skipped} skipped, {} records", out.len());
        let r = &out.first().ok_or_else(|| anyhow::anyhow!("no record"))?.1;
        anyhow::ensure!(r.source == "azure.web" && r.service == "CHECKOUT" && r.severity == 9 && r.message == "GET /cart 200" && r.ts == 1_792_389_600_123_456_700, "{r:?}");
        let has = |r: &Record, k: &str, v: &str| r.attrs.iter().any(|(a, b)| a == k && b == v);
        anyhow::ensure!(has(r, "subscription_id", "S-1") && has(r, "resource_group", "RG-1") && has(r, "properties.ScStatus", "200") && has(r, "category", "AppServiceHTTPLogs") && has(r, "team", "pay") && has(r, "azure.event_hub", "insights-logs"), "{:?}", r.attrs);
        let vm = &out.get(1).ok_or_else(|| anyhow::anyhow!("no record"))?.1;
        anyhow::ensure!(vm.source == "azure.compute" && vm.service == "vm-1" && vm.severity == 17 && vm.message == "MICROSOFT.COMPUTE/VIRTUALMACHINES/DELETE", "a failed operation is an error: {vm:?}");
        let signin = &out.get(2).ok_or_else(|| anyhow::anyhow!("no record"))?.1;
        anyhow::ensure!(signin.source == "azure.aadiam" && has(signin, "tenant", "t-1") && signin.message == "Sign-in activity", "{signin:?}");
        // Each record's id is its event's place and its index: the same event read again, the same ids.
        let mut again = Vec::new();
        records_of(event.to_string().as_bytes(), 1, place(7), &tags, &mut again);
        anyhow::ensure!(out.iter().map(|x| x.0).collect::<Vec<_>>() == again.iter().map(|x| x.0).collect::<Vec<_>>());
        let mut other = Vec::new();
        records_of(event.to_string().as_bytes(), 1, place(8), &tags, &mut other);
        anyhow::ensure!(other.first().map(|x| x.0) != out.first().map(|x| x.0));
        // Not JSON: the event's text, at its own time.
        let mut plain = Vec::new();
        records_of(b"ERROR something broke", 1_800_000_000_000, place(9), &[], &mut plain);
        let p = &plain.first().ok_or_else(|| anyhow::anyhow!("no record"))?.1;
        anyhow::ensure!(p.message == "ERROR something broke" && p.severity == 17 && p.ts == 1_800_000_000_000_000_000 && p.source == "azure", "{p:?}");
        Ok(())
    }

    #[test]
    fn settings_are_checked() {
        let c = Config { namespace: "kiln-logs".into(), hubs: vec!["insights-activity-logs".into()], connection_string: None, endpoint: None, tls: true };
        assert_eq!(c.check(), Ok(()));
        assert_eq!(c.bootstrap(), ("kiln-logs.servicebus.windows.net".to_owned(), 9093));
        assert!(Config { tls: false, ..c.clone() }.check().is_err(), "no TLS to Azure");
        assert_eq!(Config { tls: false, endpoint: Some("127.0.0.1:19092".into()), ..c.clone() }.bootstrap(), ("127.0.0.1".to_owned(), 19092));
        assert!(Config { hubs: vec!["a b".into()], ..c.clone() }.check().is_err());
        assert!(Config { connection_string: Some("nope".into()), ..c.clone() }.check().is_err());
        assert_eq!(Config { namespace: "x.servicebus.chinacloudapi.cn".into(), ..c }.host(), "x.servicebus.chinacloudapi.cn");
    }
}
