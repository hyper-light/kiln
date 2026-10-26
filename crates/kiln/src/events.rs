//! kiln's events (docs/bus.md, "kiln's events"; CP-T2.2 step 8, AC-T2.2.1): recorded with the
//! change they report, relayed in order to the bus's persisted topic [`TOPIC`], and delivered from
//! it, so a kill -9 of the server loses none.
//!
//! - **Recorded** ([`record`]): a row of `event_outbox`, in the SQLite transaction that makes the
//!   change it reports (the transactional outbox). A run's or a job's change of status is recorded
//!   by triggers on `runs` and `jobs` (the migration `cp_t2_2_event_outbox`); a run's end, a
//!   deployment's, a run waiting for its reviewers and a land queue's ejection by the code that makes
//!   them. What isn't a change of kiln.db is recorded as it's said ([`say`]).
//! - **Relayed** ([`spawn`]): oldest row first, each row's events published to [`TOPIC`] with the
//!   row's number in them ([`Number`]), then the rows recorded relayed and let go. A restart goes on
//!   from the first row not recorded; a row the topic already ends with is not published again.
//! - **Delivered** from the topic by each kind of delivery, from a position of its own
//!   (`crate::ci::webhooks`, `crate::ci::notify`), skipping an event whose number it has passed.

use std::sync::{Arc, Weak};
use std::time::Duration;

use kiln_seams::bus::{Bus as _, Class, Refusal, Topic};
use rusqlite::{Connection, OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::hub::Hub;

/// The persisted topic kiln's events ride.
pub const TOPIC: &str = "kiln.events";
/// Messages [`TOPIC`] keeps at most (Quiver's bound); its bytes are `[bus] persisted_bytes`.
pub const RETAIN: usize = 1_000_000;
/// Outbox rows the relay takes at a time.
const BATCH: usize = 256;
/// How long the relay rests when nothing woke it. Whatever the hub tells its viewers wakes it
/// (`Hub::emit`), as a change a trigger recorded is mostly followed by; it looks again this often
/// for the rest.
const LOOK_EVERY: Duration = Duration::from_secs(1);
/// The longest the relay waits before trying again after it couldn't go on (the bus's persisted
/// class not open, a refusal): it starts at a tenth of a second and doubles.
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// An event's number: the outbox row it came from, and its place among that row's events. Every
/// event kiln makes has its own, in the order they were recorded, and keeps it however often it's
/// published.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Number(pub i64, pub u32);

impl std::fmt::Display for Number {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.0, self.1)
    }
}

/// An event on [`TOPIC`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub n: Number,
    /// When the change it reports committed (or it was said): unix milliseconds.
    pub at: i64,
    /// What it is, as outgoing webhooks and notification rules name it: `run`, `job`,
    /// `run.failed`, `deploy.succeeded`, `cd_apps`, `agent.needs_you`, …
    #[serde(rename = "type")]
    pub kind: String,
    /// What it's about.
    #[serde(flatten)]
    pub about: Map<String, Value>,
}

/// Events a message on [`TOPIC`] carries at most, and its bytes at most: the relay publishes a
/// pass's events in as few messages as these allow, each a flush of the topic's log, in order.
const MESSAGE_EVENTS: usize = 128;
const MESSAGE_BYTES: usize = 256 << 10;

/// A message on [`TOPIC`]: events, oldest first.
pub fn encode(events: &[Event]) -> Vec<u8> {
    serde_json::to_vec(events).unwrap_or_default()
}

/// The events a message read off the topic carries, if it's one of kiln's.
pub fn decode(payload: &[u8]) -> Option<Vec<Event>> {
    serde_json::from_slice(payload).ok()
}

/// Records an event in the transaction `c` is in: call it in the transaction that makes the change
/// the event reports, so both commit or neither does. `kind` says how the relay reads `fact`.
pub fn record(c: &Connection, kind: &str, fact: &Value) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO event_outbox (at, kind, fact) VALUES (CAST(unixepoch('subsec') * 1000 AS INTEGER), ?1, ?2)",
        params![kind, fact.to_string()],
    )?;
    Ok(())
}

/// Records `event` (its `type` and what it's about) as said now, in a transaction of its own, and
/// wakes the relay: an event that reports no change of kiln.db (KilnCD's apps, a runner's comings
/// and goings, an agent's turn, a review asked for).
pub async fn say(hub: &Hub, event: Value) {
    let kind = event.get("type").and_then(Value::as_str).unwrap_or_default().to_owned();
    if let Err(e) = hub.store.call(move |c| Ok(record(c, "said", &event)?)).await {
        tracing::warn!(target: "kiln.events", event = "unrecorded", kind = %kind, error = %format!("{e:#}"), "an event couldn't be recorded, so it won't be delivered");
        crate::metrics::events().unrecorded.inc();
    }
    hub.relay.kick();
}

/// What the hub tells its viewers of that outgoing webhooks take and nothing else records: kept in
/// order for the relay to record, at most [`SAID_LATER`] at once.
const FROM_EMIT: &[&str] = &["agent", "queue", "cd_plan", "cd_apps", "cd_app_deleted"];
/// Events the hub said and the relay hasn't recorded yet, past which the newest are refused,
/// counted (`kiln_events_unrecorded_total`).
const SAID_LATER: usize = 10_000;

/// What wakes the relay: a row recorded where the code knows it (the triggers' rows it finds by
/// looking, every [`LOOK_EVERY`]); and the events the hub said, which it records first.
#[derive(Default)]
pub struct Relay {
    wake: crate::rt::Notify,
    said: parking_lot::Mutex<std::collections::VecDeque<Value>>,
}

impl Relay {
    pub fn kick(&self) {
        self.wake.notify();
    }

    /// What the hub tells its viewers (`Hub::emit`), kept for the relay to record when outgoing
    /// webhooks take it and nothing else records it ([`FROM_EMIT`]).
    pub fn heard(&self, v: &Value) {
        if !v.get("type").and_then(Value::as_str).is_some_and(|t| FROM_EMIT.contains(&t)) {
            return;
        }
        let mut said = self.said.lock();
        if said.len() >= SAID_LATER {
            crate::metrics::events().unrecorded.inc();
            return;
        }
        said.push_back(v.clone());
    }
}

/// One outbox row.
struct Row {
    id: i64,
    at: i64,
    kind: String,
    fact: Value,
}

/// Where the relay stops short, for the tests of what a kill at each point leaves (CP-T2.2 step 8:
/// between a change's commit and its event's publish, and between the publish and its record).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// Rows read, none published.
    BeforePublish,
    /// Rows published, not recorded relayed.
    BeforeRecord,
}

/// Relays the outbox to [`TOPIC`] for as long as the hub lives, once `readers` (the readers this
/// server runs) stand on the topic: a reader new to it starts at its end, and so, placed before the
/// relay publishes anything, it misses nothing this start makes. On a task of its own: the server's
/// start waits for neither.
pub fn start(hub: &Arc<Hub>, readers: &'static [&'static str]) {
    let hub = Arc::clone(hub);
    crate::rt::spawn(async move {
        if hub.bus.persisted_refused().is_none() {
            match hub.bus.create(TOPIC, Topic { class: Class::Persisted, retain: RETAIN }).await {
                Ok(()) | Err(Refusal::Exists(_)) => {
                    for name in readers {
                        if let Err(e) = position(&hub, name).await {
                            tracing::warn!(target: "kiln.events", event = "unplaced", reader = name, error = %format!("{e:#}"), "a reader of kiln's events couldn't take its place; it will as it starts");
                        }
                    }
                }
                Err(e) => tracing::warn!(target: "kiln.events", event = "no_topic", error = ?e, "kiln.events couldn't be made yet; the relay goes on trying"),
            }
        }
        spawn(&hub);
    });
}

/// Relays the outbox to [`TOPIC`] for as long as the hub lives.
pub fn spawn(hub: &Arc<Hub>) {
    let weak = Arc::downgrade(hub);
    crate::rt::spawn(async move { relay(weak).await });
}

async fn relay(weak: Weak<Hub>) {
    let mut ready = false;
    let mut backoff = Duration::from_millis(100);
    let mut said: Option<String> = None;
    loop {
        let Some(hub) = weak.upgrade() else { return };
        let rest = match relay_once(&hub, &mut ready, None).await {
            Ok(n) => {
                backoff = Duration::from_millis(100);
                if said.take().is_some() {
                    tracing::info!(target: "kiln.events", event = "relaying", "the relay goes on");
                }
                (n == 0).then_some(LOOK_EVERY)
            }
            Err(e) => {
                let why = format!("{e:#}");
                if said.as_deref() != Some(why.as_str()) {
                    tracing::warn!(target: "kiln.events", event = "relay_stopped", error = %why, "kiln's events wait in the outbox until the relay can go on");
                    said = Some(why);
                }
                let rest = backoff;
                backoff = (backoff * 2).min(BACKOFF_MAX);
                Some(rest)
            }
        };
        let Some(rest) = rest else { continue };
        let (clock, woken) = (Arc::clone(&hub.clock), Arc::clone(&hub));
        drop(hub);
        let woke = Box::pin(async move { woken.relay.wake.notified().await });
        futures::future::select(woke, clock.sleep(rest)).await;
    }
}

/// One pass: the topic made (and its tail read) the first time, then up to [`BATCH`] rows
/// published and recorded relayed. How many rows it relayed.
pub(crate) async fn relay_once(hub: &Hub, ready: &mut bool, stop: Option<Stop>) -> anyhow::Result<usize> {
    // What the hub said meanwhile, recorded first, in the order it was said.
    let said: Vec<Value> = hub.relay.said.lock().drain(..).collect();
    if !said.is_empty() {
        let kept = said.clone();
        if let Err(e) = hub.store.call(move |c| {
            let tx = crate::store::tx(c)?;
            for v in &kept {
                record(&tx, "said", v)?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
        {
            // Back at the front, to be recorded on the next pass.
            let mut queued = hub.relay.said.lock();
            for v in said.into_iter().rev() {
                queued.push_front(v);
            }
            return Err(e.context("recording what the hub said"));
        }
    }
    if let Some(why) = hub.bus.persisted_refused() {
        anyhow::bail!("the bus's persisted class didn't open: {why}");
    }
    if !*ready {
        match hub.bus.create(TOPIC, Topic { class: Class::Persisted, retain: RETAIN }).await {
            Ok(()) | Err(Refusal::Exists(_)) => {}
            Err(e) => anyhow::bail!("making {TOPIC}: {e:?}"),
        }
        // Rows published before a kill kept them from being recorded relayed: the topic ends with
        // the last of them, whose events may have gone only in part, so it goes again whole (its
        // events with the numbers they had: a delivery skips what it has passed), the rest before
        // it not again.
        let last = last_on_topic(hub).await?;
        let (unrecorded, waiting) = hub
            .store
            .call(move |c| {
                let recorded: i64 = c.query_row("SELECT relayed FROM event_relay", [], |r| r.get(0))?;
                if let Some(last) = last {
                    c.execute("UPDATE event_relay SET relayed = MAX(relayed, ?1)", [last.saturating_sub(1)])?;
                    c.execute("DELETE FROM event_outbox WHERE id <= (SELECT relayed FROM event_relay)", [])?;
                }
                let waiting: i64 = c.query_row("SELECT COUNT(*) FROM event_outbox", [], |r| r.get(0))?;
                Ok((last.map_or(0, |l| l.saturating_sub(recorded).max(0)), waiting))
            })
            .await?;
        // What the last stop left: rows it published and never recorded so, and rows to publish.
        tracing::info!(target: "kiln.events", event = "relay_start", unrecorded, waiting, "the relay starts");
        *ready = true;
    }
    let rows: Vec<Row> = hub
        .store
        .call(|c| {
            let mut st = c.prepare("SELECT id, at, kind, fact FROM event_outbox WHERE id > (SELECT relayed FROM event_relay) ORDER BY id LIMIT ?1")?;
            let rows = st.query_map([i64::try_from(BATCH).unwrap_or(i64::MAX)], |r| {
                let fact: String = r.get(3)?;
                Ok(Row { id: r.get(0)?, at: r.get(1)?, kind: r.get(2)?, fact: serde_json::from_str(&fact).unwrap_or(Value::Null) })
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await?;
    let Some(last) = rows.last().map(|r| r.id) else { return Ok(0) };
    if stop == Some(Stop::BeforePublish) {
        return Ok(0);
    }
    let mut message = Message::default();
    for row in &rows {
        let events = derive(hub, &row.kind, &row.fact).await?;
        // The hub's viewers hear of a notification's event made here as `notify::emit` tells them
        // of one said: once it's on its way.
        let shown = !matches!(row.kind.as_str(), "run" | "job" | "said");
        for (i, (kind, about)) in events.into_iter().enumerate() {
            let e = Event { n: Number(row.id, u32::try_from(i).unwrap_or(u32::MAX)), at: row.at, kind, about };
            let size = serde_json::to_vec(&e).map_or(0, |b| b.len());
            if !message.events.is_empty() && (message.events.len() >= MESSAGE_EVENTS || message.bytes.saturating_add(size) > MESSAGE_BYTES) {
                message.publish(hub).await?;
            }
            if shown && let Some(body) = crate::ci::notify::shown(&e) {
                message.shown.push(body);
            }
            message.bytes = message.bytes.saturating_add(size);
            message.events.push(e);
        }
    }
    message.publish(hub).await?;
    if stop == Some(Stop::BeforeRecord) {
        return Ok(0);
    }
    hub.store
        .call(move |c| {
            c.execute("UPDATE event_relay SET relayed = ?1", [last])?;
            c.execute("DELETE FROM event_outbox WHERE id <= ?1", [last])?;
            Ok(())
        })
        .await?;
    let m = crate::metrics::events();
    m.relayed.add(u64::try_from(rows.len()).unwrap_or(0));
    let waiting = if rows.len() < BATCH { 0 } else { hub.store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM event_outbox", [], |r| r.get::<_, i64>(0))?)).await? };
    m.outbox.set(waiting);
    Ok(rows.len())
}

/// The events of a pass the relay has yet to publish, as one message; and what the hub's viewers
/// hear of them once they're published.
#[derive(Default)]
struct Message {
    events: Vec<Event>,
    bytes: usize,
    shown: Vec<Value>,
}

impl Message {
    /// Publishes the events kept, as one message (each alone, should the bus refuse them together
    /// as too large), tells the hub's viewers, and starts the next.
    async fn publish(&mut self, hub: &Hub) -> anyhow::Result<()> {
        if self.events.is_empty() {
            return Ok(());
        }
        let events = std::mem::take(&mut self.events);
        match publish(hub, &events).await {
            Err(Refusal::TooLarge { .. }) if events.len() > 1 => {
                for e in events {
                    match publish(hub, std::slice::from_ref(&e)).await {
                        Ok(()) => {}
                        Err(r) => Self::refused(&e, r)?,
                    }
                }
            }
            Ok(()) => {}
            Err(r) => {
                let first = events.first().map(|e| e.n);
                if let (Some(e), Refusal::TooLarge { .. }) = (events.first(), &r) {
                    Self::refused(e, r)?;
                } else {
                    anyhow::bail!("publishing events from {first:?} to {TOPIC}: {r:?}");
                }
            }
        }
        for body in self.shown.drain(..) {
            hub.emit(&body);
        }
        self.bytes = 0;
        Ok(())
    }

    /// An event the bus refused alone: one larger than a message may be can never go, so it's told
    /// and counted and the rest go on; any other refusal stops the pass, to go again.
    fn refused(e: &Event, r: Refusal) -> anyhow::Result<()> {
        match r {
            Refusal::TooLarge { size, limit, .. } => {
                tracing::warn!(target: "kiln.events", event = "too_large", n = %e.n, size, limit, "an event is larger than a message on kiln.events may be: it won't be delivered");
                crate::metrics::events().unrecorded.inc();
                Ok(())
            }
            r => anyhow::bail!("publishing event {} to {TOPIC}: {r:?}", e.n),
        }
    }
}

/// Publishes `events` as one message on [`TOPIC`].
async fn publish(hub: &Hub, events: &[Event]) -> Result<(), Refusal> {
    let key = events.first().map(|e| e.n.to_string().into_bytes());
    hub.bus.publish(TOPIC, key, encode(events)).await?;
    crate::metrics::events().published.add(u64::try_from(events.len()).unwrap_or(0));
    Ok(())
}

/// The outbox row the topic's last event came from, if the topic has one.
async fn last_on_topic(hub: &Hub) -> anyhow::Result<Option<i64>> {
    let (first, end) = hub.bus.bounds(TOPIC).await.map_err(|r| anyhow::anyhow!("{TOPIC}'s bounds: {r:?}"))?;
    if end <= first {
        return Ok(None);
    }
    let read = hub.bus.read(TOPIC, end - 1, 1).await.map_err(|r| anyhow::anyhow!("{TOPIC}'s last event: {r:?}"))?;
    Ok(read.messages.first().and_then(|m| decode(&m.payload)).and_then(|events| events.last().map(|e| e.n.0)))
}

/// The events an outbox row makes, each its type and what it's about, in order.
async fn derive(hub: &Hub, kind: &str, fact: &Value) -> anyhow::Result<Vec<(String, Map<String, Value>)>> {
    let object = |v: &Value| v.as_object().cloned().unwrap_or_default();
    let text = |key: &str| fact.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
    let texts = |key: &str| -> Vec<String> { fact.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default() };
    Ok(match kind {
        // A run's or a job's change, as its trigger recorded it.
        "run" | "job" => vec![(kind.to_owned(), object(fact))],
        // An event said as it was made: its type is in it.
        "said" => {
            let mut about = object(fact);
            match about.remove("type") {
                Some(Value::String(kind)) => vec![(kind, about)],
                _ => Vec::new(),
            }
        }
        "run.finished" => crate::ci::notify::finished(hub, &text("run")).await?,
        "deployed" => crate::ci::notify::deployment(hub, &text("run"), &text("environment"), fact.get("succeeded").and_then(Value::as_bool).unwrap_or(false)).await?,
        "approval.waiting" => crate::ci::notify::waiting(hub, &text("run"), texts("environments")).await?,
        "land.ejected" => crate::ci::notify::ejected(hub, texts("diffs"), text("reason")).await?,
        other => {
            // A kind a later build records (a rollback meets it): told, and passed over.
            tracing::warn!(target: "kiln.events", event = "unknown_kind", kind = %other, "an outbox row of a kind this build doesn't know; it makes no event");
            crate::metrics::events().unknown.inc();
            Vec::new()
        }
    })
}

/// A notification-style event (what a rule's `on` names) as the relay publishes it: its repository
/// and its subject.
pub fn about(repo: &crate::store::Repo, subject: &impl Serialize) -> Map<String, Value> {
    let mut about = Map::new();
    about.insert("repo".into(), json!({ "id": repo.id, "name": repo.name }));
    about.insert("subject".into(), serde_json::to_value(subject).unwrap_or(Value::Null));
    about
}

// ------------------------------------------------------------------------------- delivering

/// Messages a reader takes from the topic at a time.
const READ_BATCH: usize = 64;
/// How long a reader waits on the topic before it looks again (its hub gone, say).
const READ_WAIT: Duration = Duration::from_secs(5);
/// How long a delivery is tried before it's given up as failed: a day (docs/ci.md W19-R07; the
/// Standard Webhooks spec's "a retry schedule spanning multiple days, with an exponential backoff"
/// is the longer end of the same advice).
pub const RETRY_FOR: Duration = Duration::from_secs(24 * 3600);
/// The longest wait between two tries of a delivery: the waits double from 2 s up to it.
pub const BACKOFF_CAP: Duration = Duration::from_secs(300);
/// How often an idle sender looks at its log when nothing woke it.
const SENDER_LOOK_EVERY: Duration = Duration::from_secs(60);

/// The wait before a delivery's next try, after `tries` tries: 2 s, 4 s, 8 s… up to
/// [`BACKOFF_CAP`].
pub fn backoff(tries: i64) -> Duration {
    let doubled = u32::try_from(tries.clamp(1, 16)).map_or(u64::MAX, |n| 1u64 << n);
    Duration::from_secs(doubled).min(BACKOFF_CAP)
}

/// The deliveries one event makes, worked out (lookups and all) before the transaction that logs
/// them and moves the reader past the event: what writes them, and the targets whose senders to
/// wake.
pub struct Logged {
    pub write: Box<dyn FnOnce(&Connection) -> rusqlite::Result<()> + Send>,
    pub targets: Vec<String>,
}

impl Logged {
    /// No delivery: the reader only moves past the event.
    pub fn none() -> Logged {
        Logged { write: Box::new(|_| Ok(())), targets: Vec::new() }
    }
}

/// A reader of [`TOPIC`] whose deliveries are logged in kiln.db and sent by a [`Senders`] for its
/// kind (`crate::ci::webhooks`, `crate::ci::notify`).
pub trait Reader: Send + Sync + 'static {
    /// Its position's name in `event_positions`.
    fn name(&self) -> &'static str;
    /// The deliveries `e` makes.
    fn plan<'a>(&'a self, hub: &'a Arc<Hub>, e: &'a Event) -> futures::future::BoxFuture<'a, anyhow::Result<Logged>>;
    /// Wakes the senders of `targets`, their deliveries logged.
    fn wake(&self, hub: &Arc<Hub>, targets: Vec<String>);
}

/// Reads [`TOPIC`] for `reader`, for as long as the hub lives.
pub fn read<R: Reader>(hub: &Arc<Hub>, reader: Arc<R>) {
    let weak = Arc::downgrade(hub);
    crate::rt::spawn(async move { read_loop(weak, reader).await });
}

async fn read_loop<R: Reader>(weak: Weak<Hub>, reader: Arc<R>) {
    let mut backoff = Duration::from_millis(100);
    let mut said: Option<String> = None;
    loop {
        let Some(hub) = weak.upgrade() else { return };
        match read_once(&hub, reader.as_ref()).await {
            Ok(_) => {
                backoff = Duration::from_millis(100);
                if said.take().is_some() {
                    tracing::info!(target: "kiln.events", event = "reading", reader = reader.name(), "the reader goes on");
                }
            }
            Err(e) => {
                let why = format!("{e:#}");
                if said.as_deref() != Some(why.as_str()) {
                    tracing::warn!(target: "kiln.events", event = "reader_stopped", reader = reader.name(), error = %why, "deliveries wait on the topic until the reader can go on");
                    said = Some(why);
                }
                let (rest, clock) = (backoff, Arc::clone(&hub.clock));
                drop(hub);
                clock.sleep(rest).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
}

/// Where reader `name` stands: the offset it reads next, and the number of the last event it took.
/// A reader new to the topic starts at its end, so its deliveries are of what comes after.
async fn position(hub: &Hub, name: &'static str) -> anyhow::Result<(u64, Number)> {
    let kept: Option<(i64, i64, i64)> = hub
        .store
        .call(move |c| Ok(c.query_row("SELECT next, passed_row, passed_index FROM event_positions WHERE reader = ?1", [name], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?))
        .await?;
    if let Some((next, row, index)) = kept {
        return Ok((u64::try_from(next).unwrap_or(0), Number(row, u32::try_from(index).unwrap_or(0))));
    }
    let (_, end) = hub.bus.bounds(TOPIC).await.map_err(|r| anyhow::anyhow!("{TOPIC}'s bounds: {r:?}"))?;
    let (at, now) = (i64::try_from(end).unwrap_or(i64::MAX), hub.clock.unix_s());
    hub.store.call(move |c| Ok(c.execute("INSERT OR IGNORE INTO event_positions (reader, next, passed_row, passed_index, updated_at) VALUES (?1, ?2, 0, 0, ?3)", params![name, at, now]).map(|_| ())?)).await?;
    Ok((end, Number(0, 0)))
}

/// One pass of `reader`: the events after its position, each one's deliveries logged and its
/// position moved past it in one transaction; or, with none, a wait for the next. How many it took.
async fn read_once<R: Reader>(hub: &Arc<Hub>, reader: &R) -> anyhow::Result<usize> {
    if let Some(why) = hub.bus.persisted_refused() {
        anyhow::bail!("the bus's persisted class didn't open: {why}");
    }
    let name = reader.name();
    let (next, mut passed) = position(hub, name).await?;
    let batch = match hub.bus.read(TOPIC, next, READ_BATCH).await {
        Ok(batch) => batch,
        Err(Refusal::Behind { asked, first }) => {
            // The topic let go of events this reader never took: told and counted, never skipped
            // silently (a reader stopped longer than the topic keeps).
            let missed = first.saturating_sub(asked);
            tracing::warn!(target: "kiln.events", event = "behind", reader = name, asked, first, missed, "a reader of kiln's events fell behind what the topic keeps: those events make no delivery");
            crate::metrics::events().missed.add(&[name], missed);
            let (at, now) = (i64::try_from(first).unwrap_or(i64::MAX), hub.clock.unix_s());
            hub.store.call(move |c| Ok(c.execute("UPDATE event_positions SET next = ?2, updated_at = ?3 WHERE reader = ?1", params![name, at, now]).map(|_| ())?)).await?;
            return Ok(0);
        }
        // The relay hasn't made the topic yet.
        Err(Refusal::NoTopic(_)) => {
            hub.relay.kick();
            let clock = Arc::clone(&hub.clock);
            clock.sleep(Duration::from_millis(200)).await;
            return Ok(0);
        }
        Err(e) => anyhow::bail!("reading {TOPIC} at {next}: {e:?}"),
    };
    if batch.messages.is_empty() {
        let _ = crate::rt::timeout(READ_WAIT, hub.bus.wait(TOPIC, next)).await;
        return Ok(0);
    }
    let took = batch.messages.len();
    for m in batch.messages {
        let events = decode(&m.payload).unwrap_or_else(|| {
            tracing::warn!(target: "kiln.events", event = "unreadable", reader = name, offset = m.offset, "a message on kiln.events that isn't kiln's events: passed over");
            Vec::new()
        });
        // The message's deliveries, every event's but those it took before a kill (published
        // again), logged with its position past the message in one transaction.
        let (mut writes, mut targets) = (Vec::new(), Vec::new());
        let mut n = passed;
        for e in events.iter().filter(|e| e.n > passed) {
            let logged = reader.plan(hub, e).await?;
            writes.push(logged.write);
            targets.extend(logged.targets);
            n = n.max(e.n);
        }
        let (at, now) = (i64::try_from(m.offset.saturating_add(1)).unwrap_or(i64::MAX), hub.clock.unix_s());
        hub.store
            .call(move |c| {
                let tx = crate::store::tx(c)?;
                for write in writes {
                    write(&tx)?;
                }
                tx.execute("UPDATE event_positions SET next = ?2, passed_row = ?3, passed_index = ?4, updated_at = ?5 WHERE reader = ?1", params![name, at, n.0, n.1, now])?;
                tx.commit()?;
                Ok(())
            })
            .await?;
        targets.sort();
        targets.dedup();
        reader.wake(hub, targets);
        passed = n;
    }
    Ok(took)
}

/// A delivery due to be tried: its row in its log, when it was logged, when it may go, and how
/// many tries it had.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Due {
    pub row: i64,
    pub first_at: i64,
    pub next_at: i64,
    pub tries: i64,
}

/// How one try of a delivery went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tried {
    Sent { code: Option<u16> },
    /// Not this time: tried again after [`backoff`], until [`RETRY_FOR`] after it was logged.
    Failed { code: Option<u16>, error: String },
    /// Done however it went: one that can't go however long it waits (its integration gone, a
    /// tracker's change, which retries itself), or that its channel had nothing to do with.
    Done { status: &'static str, code: Option<u16>, error: Option<String> },
}

/// Where a delivery stands after a try: its status (`pending`, with when it may go next), the
/// code and error the try had, and when it was.
pub struct After<'a> {
    pub status: &'a str,
    pub code: Option<u16>,
    pub error: Option<&'a str>,
    pub next_at: Option<i64>,
    pub now: i64,
}

/// A kind of delivery's log, as its senders use it.
pub trait Log: Send + Sync + 'static {
    /// Its name in metrics.
    fn name(&self) -> &'static str;
    /// The oldest pending delivery of `target`.
    fn due(&self, c: &Connection, target: &str) -> rusqlite::Result<Option<Due>>;
    /// Tries delivery `row` once.
    fn send<'a>(&'a self, hub: &'a Arc<Hub>, row: i64) -> futures::future::BoxFuture<'a, Tried>;
    /// Records a try of `row`.
    fn tried(&self, c: &Connection, row: i64, after: &After<'_>) -> rusqlite::Result<()>;
    /// The targets with deliveries pending: what a restart left.
    fn pending(&self, c: &Connection) -> rusqlite::Result<Vec<String>>;
    /// How many deliveries are pending, every target's.
    fn waiting(&self, c: &Connection) -> rusqlite::Result<i64>;
}

/// A sender for each target of a kind of delivery, each sending its target's deliveries one at a
/// time, oldest first, so a target that's down holds up only itself.
pub struct Senders<L: Log> {
    log: Arc<L>,
    running: parking_lot::Mutex<std::collections::HashMap<String, Arc<crate::rt::Notify>>>,
}

impl<L: Log> Senders<L> {
    pub fn new(log: L) -> Arc<Self> {
        Arc::new(Senders { log: Arc::new(log), running: parking_lot::Mutex::new(std::collections::HashMap::new()) })
    }

    pub fn log(&self) -> &L {
        &self.log
    }

    /// Wakes `target`'s sender, starting it if it has none.
    pub fn wake(self: &Arc<Self>, hub: &Arc<Hub>, target: &str) {
        let wake = {
            let mut running = self.running.lock();
            if let Some(wake) = running.get(target) {
                Some(Arc::clone(wake))
            } else {
                running.insert(target.to_owned(), Arc::new(crate::rt::Notify::new()));
                None
            }
        };
        match wake {
            Some(wake) => wake.notify(),
            None => {
                let (weak, senders, target) = (Arc::downgrade(hub), Arc::clone(self), target.to_owned());
                crate::rt::spawn(async move { senders.send_loop(weak, target).await });
            }
        }
    }

    /// Wakes the senders of every target with deliveries pending (as a reader starts).
    pub async fn resume(self: &Arc<Self>, hub: &Arc<Hub>) -> anyhow::Result<()> {
        let log = Arc::clone(&self.log);
        let (targets, waiting) = hub.store.call(move |c| Ok((log.pending(c)?, log.waiting(c)?))).await?;
        tracing::info!(target: "kiln.events", event = "senders_start", kind = self.log.name(), targets = targets.len(), waiting, "the senders start");
        for target in targets {
            self.wake(hub, &target);
        }
        Ok(())
    }

    async fn send_loop(self: Arc<Self>, weak: Weak<Hub>, target: String) {
        let Some(wake) = self.running.lock().get(&target).cloned() else { return };
        let m = crate::metrics::events();
        let name = self.log.name();
        loop {
            let Some(hub) = weak.upgrade() else { return };
            let (log, t) = (Arc::clone(&self.log), target.clone());
            let due = match hub.store.call(move |c| Ok(log.due(c, &t)?)).await {
                Ok(due) => due,
                Err(e) => {
                    tracing::warn!(target: "kiln.events", event = "sender_stopped", kind = name, target = %target, error = %format!("{e:#}"), "a delivery's log couldn't be read");
                    None
                }
            };
            let clock = Arc::clone(&hub.clock);
            let now = clock.unix_s();
            let Some(due) = due else {
                drop(hub);
                let woke = Box::pin(wake.notified());
                futures::future::select(woke, clock.sleep(SENDER_LOOK_EVERY)).await;
                continue;
            };
            if due.next_at > now {
                drop(hub);
                clock.sleep(Duration::from_secs(u64::try_from(due.next_at - now).unwrap_or(0))).await;
                continue;
            }
            let tried = self.log.send(&hub, due.row).await;
            let now = clock.unix_s();
            let tries = due.tries.saturating_add(1);
            let (status, code, error, next_at): (&str, Option<u16>, Option<String>, Option<i64>) = match tried {
                Tried::Sent { code } => ("sent", code, None, None),
                Tried::Done { status, code, error } => (status, code, error, None),
                Tried::Failed { code, error } => {
                    let given_up = now.saturating_sub(due.first_at) >= i64::try_from(RETRY_FOR.as_secs()).unwrap_or(i64::MAX);
                    if given_up {
                        tracing::warn!(target: "kiln.events", event = "given_up", kind = name, target = %target, row = due.row, tries, error = %error, "a delivery was tried for a day and given up");
                        ("failed", code, Some(format!("{error} (given up after {tries} tries over a day)")), None)
                    } else {
                        let wait = i64::try_from(backoff(tries).as_secs()).unwrap_or(i64::MAX);
                        ("pending", code, Some(error), Some(now.saturating_add(wait)))
                    }
                }
            };
            m.attempts.inc(&[name, if status == "pending" { "retry" } else { status }]);
            if status == "sent" {
                m.delivery_seconds.observe(&[name], f64::from(i32::try_from(now.saturating_sub(due.first_at)).unwrap_or(i32::MAX)));
            }
            let (log, row, error) = (Arc::clone(&self.log), due.row, error);
            let status = status.to_owned();
            let recorded = hub
                .store
                .call(move |c| {
                    log.tried(c, row, &After { status: &status, code, error: error.as_deref(), next_at, now })?;
                    Ok(log.waiting(c)?)
                })
                .await;
            match recorded {
                Ok(waiting) => m.pending.replace_owned(&std::iter::once(name.to_owned()).collect(), [(vec![name.to_owned()], waiting)]),
                Err(e) => {
                    tracing::warn!(target: "kiln.events", event = "unrecorded_try", kind = name, target = %target, row, error = %format!("{e:#}"), "a delivery's try couldn't be recorded: it goes again");
                    drop(hub);
                    clock.sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::hub::tests::restartable_hub;

    /// What kiln.events holds, oldest first: each event's number and type.
    async fn on_topic(hub: &Hub) -> anyhow::Result<Vec<(Number, String)>> {
        let (first, end) = hub.bus.bounds(TOPIC).await.map_err(|r| anyhow::anyhow!("{r:?}"))?;
        let mut held = Vec::new();
        let mut at = first;
        while at < end {
            let batch = hub.bus.read(TOPIC, at, 1000).await.map_err(|r| anyhow::anyhow!("{r:?}"))?;
            anyhow::ensure!(!batch.messages.is_empty(), "nothing at {at} of {end}");
            for m in &batch.messages {
                let events = decode(&m.payload).ok_or_else(|| anyhow::anyhow!("not kiln's events at {}", m.offset))?;
                held.extend(events.into_iter().map(|e| (e.n, e.kind)));
            }
            at = batch.next;
        }
        Ok(held)
    }

    /// The outbox's rows, oldest first: each one's number and kind.
    async fn outbox(hub: &Hub) -> anyhow::Result<Vec<(i64, String)>> {
        hub.store
            .call(|c| {
                let mut st = c.prepare("SELECT id, kind FROM event_outbox ORDER BY id")?;
                let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
                Ok(rows.collect::<Result<Vec<_>, _>>()?)
            })
            .await
    }

    /// A repository's run and its job, made, started and ended: the triggers record each change in
    /// its transaction, and the run's end, told, its `run.finished`; an event said besides.
    async fn changes(hub: &Arc<Hub>) -> anyhow::Result<()> {
        hub.store
            .call(|c| {
                c.execute("INSERT INTO repos (id, name, clone_url, created_at) VALUES ('r1', 'acme/app', '', 0)", [])?;
                c.execute("INSERT INTO runs (id, repo_id, number, pipeline, event, branch, sha, status, created_at, workflow_id) VALUES ('run1', 'r1', 1, 'ci', 'push', 'main', 'abc', 'queued', 0, 'ci.yml')", [])?;
                c.execute("INSERT INTO jobs (id, run_id, key, display, status, labels, needs, spec) VALUES ('job1', 'run1', 'build', 'build', 'queued', '[]', '[]', '{}')", [])?;
                Ok(())
            })
            .await?;
        hub.store.call(|c| Ok(c.execute("UPDATE jobs SET status = 'running' WHERE id = 'job1'", []).map(|_| ())?)).await?;
        hub.store.call(|c| Ok(c.execute("UPDATE runs SET status = 'running' WHERE id = 'run1'", []).map(|_| ())?)).await?;
        // A change that isn't of status or conclusion records nothing.
        hub.store.call(|c| Ok(c.execute("UPDATE jobs SET agent = 'r9' WHERE id = 'job1'", []).map(|_| ())?)).await?;
        hub.store.call(|c| Ok(c.execute("UPDATE jobs SET status = 'succeeded', conclusion = 'success' WHERE id = 'job1'", []).map(|_| ())?)).await?;
        crate::ci::runs::finish_run(hub, "run1", crate::store::Status::Succeeded, "success", None, true).await?;
        say(hub, json!({ "type": "agent.finished", "agent": { "id": "a1", "name": "builder" } })).await;
        Ok(())
    }

    /// Relays until the outbox is empty.
    async fn relay_all(hub: &Hub) -> anyhow::Result<()> {
        let mut ready = false;
        for _ in 0..100 {
            if relay_once(hub, &mut ready, None).await? == 0 && outbox(hub).await?.is_empty() {
                return Ok(());
            }
        }
        anyhow::bail!("the outbox never emptied")
    }

    /// A restart on what `db` and `bus` hold: the hub before stopped as a kill would stop it.
    async fn restart(hub: Arc<Hub>, db: &Path, bus: &Path) -> anyhow::Result<Arc<Hub>> {
        hub.bus.halt_persisted();
        drop(hub);
        restartable_hub(crate::rt::clock(), db, bus).await
    }

    /// CP-T2.2 step 8 (AC-T2.2.1): do change a run and a job (each change's event recorded in its
    /// transaction), stop the relay where a kill would (before it publishes; once it published,
    /// before it records so), stop the server so, and start it again on the same kiln.db and bus;
    /// expect every committed event on kiln.events, in the order committed, at least once, and
    /// only the last row published before the kill published again, with the numbers it had.
    #[test]
    fn cp_t2_2_events_committed_before_a_kill_are_on_the_topic_after_it() -> anyhow::Result<()> {
        for stop in [Stop::BeforePublish, Stop::BeforeRecord] {
            let dir = tempfile::tempdir()?;
            let (db, bus): (PathBuf, PathBuf) = (dir.path().join("kiln.db"), dir.path().join("bus"));
            crate::rt::block_on(async move {
                let hub = restartable_hub(crate::rt::clock(), &db, &bus).await?;
                changes(&hub).await?;
                let committed = outbox(&hub).await?;
                let kinds: Vec<&str> = committed.iter().map(|(_, k)| k.as_str()).collect();
                assert_eq!(kinds, ["run", "job", "job", "run", "job", "run", "run.finished", "said"], "each change recorded in order, the agent's change not");
                let mut ready = false;
                relay_once(&hub, &mut ready, Some(stop)).await?;
                let before = on_topic(&hub).await?;
                let hub = restart(hub, &db, &bus).await?;
                relay_all(&hub).await?;
                let held = on_topic(&hub).await?;
                // Every committed row's events, in order: the run's end made `run.succeeded`.
                let mut taken: Vec<(Number, String)> = Vec::new();
                for (n, kind) in &held {
                    if taken.last().is_none_or(|(last, _)| n > last) {
                        taken.push((*n, kind.clone()));
                    }
                }
                let rows: Vec<i64> = taken.iter().map(|(n, _)| n.0).collect();
                assert_eq!(rows, committed.iter().map(|(id, _)| *id).collect::<Vec<_>>(), "{stop:?}: every committed row, in order, once a reader passes over what it took");
                let types: Vec<&str> = taken.iter().map(|(_, k)| k.as_str()).collect();
                assert_eq!(types, ["run", "job", "job", "run", "job", "run", "run.succeeded", "agent.finished"], "{stop:?}");
                match stop {
                    Stop::BeforePublish => assert_eq!((before.len(), held.len()), (0, taken.len()), "nothing published twice"),
                    Stop::BeforeRecord => {
                        let last = committed.last().map(|(id, _)| *id);
                        assert_eq!(before, taken, "all published before the kill");
                        let again: Vec<&(Number, String)> = held.iter().skip(before.len()).collect();
                        assert!(again.iter().all(|(n, _)| Some(n.0) == last), "{stop:?}: only the last row again: {again:?}");
                    }
                }
                assert!(outbox(&hub).await?.is_empty(), "the relayed rows let go");
                Ok::<(), anyhow::Error>(())
            })?;
        }
        Ok(())
    }
}
