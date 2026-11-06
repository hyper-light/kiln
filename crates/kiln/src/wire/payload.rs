//! What frames carry (docs/wire.md, "What can't be read"): the JSON in a runner's and the server's
//! messages, read once, where its frame is read, into the type it is.
//!
//! Each variant of [`AgentMsg`] and [`ServerMsg`] that carries JSON names its payload's type in
//! [`payload_of`], matched without a catch-all arm, and [`payload`] reads it:
//! - a tag this build doesn't know (a newer peer's kind of message, or of something in one) is
//!   `unknown`;
//! - anything else it can't read (not JSON, cut short, a field missing or of the wrong type) is
//!   `malformed`.
//!
//! What can't be read is refused to its sender: on the channel its item is NACKed, off it its
//! sender is told with `Unread`; and it's noted (`crate::wire::unread`). Handlers take the value
//! read, and none reads JSON of its own.
//!
//! A field a known payload has that this build doesn't know is taken, as docs/wire.md says (JSON,
//! so either side can add fields): counted in `kiln_wire_ignored_fields_total{wire}`, one for each
//! such field of each message, and logged once an hour for each wire and field. It's never refused
//! or audited. serde can't say which fields it ignored inside an internally tagged enum (it reads
//! one whole first), and most of these payloads are one, so they're found by writing the value
//! read back and seeing what of what came it lacks.

use std::collections::BTreeSet;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use kiln_seams::Clock;
use parking_lot::Mutex;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::unread::{Code, Recent, Wire};
use crate::proto::{AgentMsg, ServerMsg};

/// How often each wire's unknown field is said in the log.
pub const LOG_EVERY: Duration = Duration::from_secs(3600);
/// The most wires and fields kept for when each was last said: past it, the least recent is let
/// go (and said again the next time it comes).
pub const LOGGED_MOST: usize = 1024;
/// The most unknown fields counted and said of one message.
pub const FIELDS_MOST: usize = 32;
/// The most of a tag this build doesn't know, said as what a message was.
const TAG_MOST: usize = 64;

/// Why what a message carries couldn't be read, as `crate::wire::unread::Unread` says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unreadable {
    /// Its message's variant (`Harness`, `HarnessCmd`).
    pub of: &'static str,
    /// What it was: its variant, and the kind it said when that's what this build doesn't know.
    /// Never what it said.
    pub what: String,
    pub code: Code,
    pub why: String,
    /// The agent session it was about, for a session's payload.
    pub session: Option<String>,
    /// What waits on it: told why too, so it isn't left waiting.
    pub waits: Option<Waits>,
}

impl Unreadable {
    /// About agent session `session`: its page shows it too.
    fn about(mut self: Box<Self>, session: &str) -> Box<Self> {
        self.session = Some(session.to_owned());
        self
    }

    fn waits(mut self: Box<Self>, waits: Waits) -> Box<Self> {
        self.waits = Some(waits);
        self
    }

    /// An answer saying why it couldn't be read, for what asked it: `{"error", "code"}`, the code
    /// `unknown` for a kind its reader doesn't know.
    pub fn error(&self) -> Value {
        serde_json::json!({ "error": format!("{} couldn't be read ({}): {}", self.what, self.code.name(), self.why), "code": self.code.name() })
    }
}

impl std::fmt::Display for Unreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} couldn't be read ({}): {}", self.what, self.code.name(), self.why)
    }
}

impl std::error::Error for Unreadable {}

/// What waits on a message that couldn't be read, told why beside its refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Waits {
    /// Its sender, for the answer to its question `req`: answered with why.
    Asking(u64),
    /// Whoever here asked question `req`, which it answers: told why.
    Answering(u64),
    /// The viewers of stream `stream`, which it was to open: it's said to have failed.
    Stream(String),
    /// Agent session `session`, which it was to start: said refused, so it can be dismissed.
    Opening(String),
    /// The send of message `message` (its id, read of what couldn't be read as a command) to agent
    /// session `session`, for its `epoch`: the session's inbox here says what became of it
    /// (`crate::harness::host::unread_message`), a repeat of one it took answered as one.
    Sending { session: String, message: String, epoch: u64 },
}

/// The longest message id read of a command that can't be read ([`message_of`]).
const MESSAGE_MOST: usize = 256;

/// The id of a session's message (`{"id": …}`), read of `json` as a plain value: what a command
/// that can't be read was, when that much of it can be.
fn message_of(json: &str) -> Option<String> {
    let v: Value = serde_json::from_str(json).ok()?;
    let id = v.get("id")?.as_str()?;
    (!id.is_empty() && id.len() <= MESSAGE_MOST).then(|| id.to_owned())
}

/// `json`, what a message of variant `of` carries on `wire`, read as a `T`: `unknown` for a tag
/// this build doesn't know, `malformed` for the rest it can't read. The fields of it `T` doesn't
/// have are taken, counted and said in the log ([`Ignored`]).
///
/// On a shard it's read straight into its type, with no JSON tree between, and its unknown fields are
/// looked for off the shard, by [`checker`]'s thread (goal 9, plan 01a1a87b2cec step 5: looking read each
/// runner message's JSON into a tree, the type out of that, and the type back into another tree to
/// compare, on the hub's shard). What can't be read straight is read the way it always was, so it's
/// refused, and said why, as before.
pub fn payload<T: DeserializeOwned + Serialize>(wire: Wire, of: &'static str, json: &str) -> Result<T, Box<Unreadable>> {
    if crate::rt::on_shard()
        && let Ok(read) = serde_json::from_str::<T>(json)
    {
        check_later(wire, of, json, unknown_in::<T>);
        return Ok(read);
    }
    read_by_tree(wire, of, json)
}

/// [`payload`] of JSON the reader holds and has no more use for: on a shard, read straight into its type, then handed
/// as it is to [`checker`]'s thread to look for its unknown fields in, where `payload` copies it for that (goal 9, plan
/// 01a1a87b2cec step 5: each runner payload's text was copied whole on the hub's loop for its check). The same value,
/// and the same refusal of what can't be read.
pub fn payload_owned<T: DeserializeOwned + Serialize>(wire: Wire, of: &'static str, json: String) -> Result<T, Box<Unreadable>> {
    if crate::rt::on_shard()
        && let Ok(read) = serde_json::from_str::<T>(&json)
    {
        check_owned(wire, of, json, unknown_in::<T>);
        return Ok(read);
    }
    read_by_tree(wire, of, &json)
}

/// [`payload`]'s read off a shard, and of what couldn't be read straight on one: through a JSON tree, so what can't be
/// read is refused saying why, and what of it the type doesn't keep is found as it's read.
fn read_by_tree<T: DeserializeOwned + Serialize>(wire: Wire, of: &'static str, json: &str) -> Result<T, Box<Unreadable>> {
    let came: Value = serde_json::from_str(json).map_err(|e| {
        Box::new(Unreadable {
            of,
            what: of.to_owned(),
            code: Code::Malformed,
            why: if e.is_eof() { "cut short".to_owned() } else { format!("not JSON: {e}") },
            session: None,
            waits: None,
        })
    })?;
    let read = T::deserialize(&came).map_err(|e| refused(of, &e))?;
    if let Ok(kept) = serde_json::to_value(&read) {
        say_unknown(wire, of, &unknown_fields(&came, &kept));
    }
    Ok(read)
}

/// Counts `fields`, what a message of variant `of` on `wire` carried that this build doesn't know, and
/// says each in the log that wasn't said within the hour.
fn say_unknown(wire: Wire, of: &'static str, fields: &[String]) {
    if fields.is_empty() {
        return;
    }
    let said = ignored_here().lock().note(wire, fields);
    for field in said {
        tracing::info!(wire = wire.name(), of, %field, "a field this build doesn't know, in a message it read: taken (said once an hour for each wire and field)");
    }
}

/// The fields of `json` that a `T` read of it doesn't keep, as [`payload`] finds them off a shard
/// ([`unknown_fields`]); none when it isn't a `T`.
fn unknown_in<T: DeserializeOwned + Serialize>(json: &str) -> Vec<String> {
    let Ok(came) = serde_json::from_str::<Value>(json) else { return Vec::new() };
    let Ok(read) = T::deserialize(&came) else { return Vec::new() };
    serde_json::to_value(&read).map(|kept| unknown_fields(&came, &kept)).unwrap_or_default()
}

/// A payload read on a shard, for [`checker`]'s thread to look for its unknown fields in (`find`).
struct Check {
    wire: Wire,
    of: &'static str,
    json: String,
    find: fn(&str) -> Vec<String>,
}

/// The most bytes of payloads waiting for [`checker`]'s thread: as many as the hub's loop keeps waiting
/// from runners (`crate::net::EVENTS_BYTES`), so it falls no further behind than the hub may. Past it, one
/// is let go unlooked at, counted, and said at each power of two.
const CHECKS_MOST: usize = crate::net::EVENTS_BYTES;

impl crate::memory::queue::Weigh for Check {
    /// Its place and its payload's text, with the room its buffer has past it ([`payload_owned`] hands on the
    /// buffer its frame came in).
    fn weight(&self) -> usize {
        size_of::<Check>().saturating_add(self.json.capacity())
    }
}

/// The payloads let go unlooked at for want of room ([`CHECKS_MOST`]).
static CHECKS_LET_GO: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Hands a payload read on a shard to [`checker`]'s thread to look for its unknown fields in, a copy of
/// it; where there's no such thread, they're looked for here, as before.
fn check_later(wire: Wire, of: &'static str, json: &str, find: fn(&str) -> Vec<String>) {
    if checker().is_none() {
        say_unknown(wire, of, &find(json));
        return;
    }
    check_owned(wire, of, json.to_owned(), find);
}

/// [`check_later`] of a payload the reader has no more use for: handed on as it is.
fn check_owned(wire: Wire, of: &'static str, json: String, find: fn(&str) -> Vec<String>) {
    let Some(tx) = checker() else {
        say_unknown(wire, of, &find(&json));
        return;
    };
    match tx.try_send(Check { wire, of, json, find }) {
        Ok(()) => {}
        Err(async_channel::TrySendError::Full(_)) => {
            let n = CHECKS_LET_GO.fetch_add(1, std::sync::atomic::Ordering::Relaxed).saturating_add(1);
            if n.is_power_of_two() {
                tracing::warn!(let_go = n, "runners' messages weren't looked at for fields this build doesn't know: their checker is behind");
            }
        }
        // Its thread is gone: looked for here.
        Err(async_channel::TrySendError::Closed(c)) => say_unknown(c.wire, c.of, &(c.find)(&c.json)),
    }
}

/// The thread that looks for the unknown fields of payloads read on the shards, started the first time
/// one is; none where it couldn't start (said so).
fn checker() -> Option<&'static crate::memory::queue::Sender<Check>> {
    static TX: OnceLock<Option<crate::memory::queue::Sender<Check>>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = crate::memory::queue::bounded::<Check>(&crate::memory::owners::WIRE_CHECKS, CHECKS_MOST);
        let started = std::thread::Builder::new().name("kiln-wire-check".into()).spawn(move || {
            while let Ok(c) = rx.recv_blocking() {
                say_unknown(c.wire, c.of, &(c.find)(&c.json));
            }
        });
        match started {
            Ok(_) => Some(tx),
            Err(e) => {
                tracing::warn!(error = %e, "no thread to look for fields this build doesn't know in runners' messages: looked for on the shards");
                None
            }
        }
    })
    .as_ref()
}

/// Why `T` couldn't be read from what came: a tag it doesn't know is `unknown`, the rest
/// `malformed`, said without the values that came (a secret's, say).
fn refused(of: &'static str, e: &serde_json::Error) -> Box<Unreadable> {
    let said = e.to_string();
    Box::new(match unknown_tag(&said) {
        Some(tag) => Unreadable { of, what: format!("{of} ({tag})"), code: Code::Unknown, why: said, session: None, waits: None },
        None => Unreadable { of, what: of.to_owned(), code: Code::Malformed, why: without_values(&said), session: None, waits: None },
    })
}

/// The tag serde says it doesn't know: `unknown variant `teleport`, expected …`.
fn unknown_tag(said: &str) -> Option<String> {
    let rest = said.strip_prefix("unknown variant `")?;
    let tag = rest.split('`').next()?;
    Some(tag.chars().take(TAG_MOST).collect())
}

/// serde's words without the value it found: `invalid type: string "hunter2", expected u32` is
/// `invalid type: string, expected u32`.
pub(crate) fn without_values(said: &str) -> String {
    for kind in ["invalid type: ", "invalid value: "] {
        if let Some(rest) = said.strip_prefix(kind)
            && let Some((found, expected)) = rest.split_once(", expected ")
        {
            let found = found.split(['`', '"']).next().unwrap_or_default().trim_end();
            return format!("{kind}{found}, expected {expected}");
        }
    }
    said.to_owned()
}

/// The fields of what `came` that what was read of it, written back (`kept`), lacks: a path each
/// (`info.new`, `events[].new`), at most [`FIELDS_MOST`]. A field left out of what's written back
/// when it's empty (none, false, 0, "", [], {}) counts only when it came with something in it.
pub fn unknown_fields(came: &Value, kept: &Value) -> Vec<String> {
    fn walk(came: &Value, kept: &Value, at: &str, out: &mut BTreeSet<String>) {
        match (came, kept) {
            (Value::Object(c), Value::Object(k)) => {
                for (key, v) in c {
                    if out.len() >= FIELDS_MOST {
                        return;
                    }
                    let path = if at.is_empty() { key.clone() } else { format!("{at}.{key}") };
                    match k.get(key) {
                        Some(kv) => walk(v, kv, &path, out),
                        None if !empty(v) => {
                            out.insert(path);
                        }
                        None => {}
                    }
                }
            }
            (Value::Array(c), Value::Array(k)) if c.len() == k.len() => {
                let path = format!("{at}[]");
                for (v, kv) in c.iter().zip(k) {
                    walk(v, kv, &path, out);
                }
            }
            _ => {}
        }
    }
    fn empty(v: &Value) -> bool {
        match v {
            Value::Null => true,
            Value::Bool(b) => !b,
            Value::Number(n) => n.as_f64() == Some(0.0),
            Value::String(s) => s.is_empty(),
            Value::Array(a) => a.is_empty(),
            Value::Object(o) => o.is_empty(),
        }
    }
    let mut out = BTreeSet::new();
    walk(came, kept, "", &mut out);
    out.into_iter().collect()
}

/// The fields this build doesn't know that came in payloads it read: counted, and each wire's
/// field said once an hour.
pub struct Ignored {
    clock: Arc<dyn Clock>,
    said: Recent<(Wire, String), Option<Instant>>,
}

impl Ignored {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Ignored { clock, said: Recent::new(LOGGED_MOST) }
    }

    /// `fields` of a message on `wire`: counted, and those not said within the hour returned, to
    /// say now.
    pub fn note(&mut self, wire: Wire, fields: &[String]) -> Vec<String> {
        crate::metrics::wire().ignored.add(&[wire.name()], u64::try_from(fields.len()).unwrap_or(u64::MAX));
        let now = self.clock.now();
        let mut say = Vec::new();
        for f in fields {
            let (last, _) = self.said.entry((wire, f.clone()), || None);
            if last.is_none_or(|at| now.saturating_duration_since(at) >= LOG_EVERY) {
                *last = Some(now);
                say.push(f.clone());
            }
        }
        say
    }
}

/// This process's [`Ignored`], on real time.
fn ignored_here() -> &'static Mutex<Ignored> {
    static I: OnceLock<Mutex<Ignored>> = OnceLock::new();
    I.get_or_init(|| Mutex::new(Ignored::new(crate::rt::clock())))
}

/// A message as [`payload_of`] gives it: one that carries no JSON as it came, or what one carries,
/// read into its type, with the rest of it.
pub enum Carried<M, P> {
    /// A message that carries no JSON.
    Nothing(M),
    /// What one carries, read.
    Read(P),
}

/// A message whose variants may carry JSON ([`AgentMsg`], [`ServerMsg`]), read by [`payload_of`].
pub trait Carries: Sized {
    /// Each of its variants that carries JSON, with that read into its type.
    type Read;
    /// What carries JSON of it read into its type, or why it couldn't be.
    fn read(self) -> Result<Carried<Self, Self::Read>, Box<Unreadable>>;
}

/// `msg`, with what it carries read into its type ([`payload`]): each variant that carries JSON
/// names the type here, matched without a catch-all arm. Read where its frame is read, once.
pub fn payload_of<M: Carries>(msg: M) -> Result<Carried<M, M::Read>, Box<Unreadable>> {
    msg.read()
}

/// A runner's frame, as [`read_frame`] read it.
pub enum Frame {
    /// It isn't a runner's message this build reads.
    Unreadable(crate::proto::Unreadable),
    /// A message that may not come before its runner's Hello, from a runner whose Hello hasn't come: its
    /// kind (`crate::wire::agent_name`), and nothing it carries read.
    BeforeHello { what: String },
    /// A runner's message, what it carries read ([`payload_of`]), and the label of its kind for the shards'
    /// table of what held them (`crate::wire::agent_label`), for the polls that handle it.
    Read { label: &'static hyper_rt::slow::Label, carried: Result<Carried<AgentMsg, FromRunner>, Box<Unreadable>> },
}

/// What reading a runner frame is expected to take, by its length ([`read_frame`]): moving averages of
/// each frame's bytes and of how long its read took where it ran, nanoseconds (the weight of a new
/// sample 1/8, as RFC 6298 smooths a round trip). One writer, the hub's loop.
#[derive(Default)]
struct Expect {
    bytes: std::sync::atomic::AtomicU64,
    ns: std::sync::atomic::AtomicU64,
}

impl Expect {
    /// An [`Expect`] with nothing learned.
    const fn nothing_yet() -> Self {
        Self { bytes: std::sync::atomic::AtomicU64::new(0), ns: std::sync::atomic::AtomicU64::new(0) }
    }

    /// Nanoseconds a frame of `len` bytes is expected to take to read: it grows with its length at the
    /// rate the frames so far were read, not a short one's fixed cost; none expected until one was read.
    fn expected(&self, len: u64) -> u64 {
        use std::sync::atomic::Ordering::Relaxed;
        len.saturating_mul(self.ns.load(Relaxed)).checked_div(self.bytes.load(Relaxed)).unwrap_or(0)
    }

    /// A frame of `len` bytes took `took` nanoseconds to read, on a shard or off one.
    fn learn(&self, len: u64, took: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        // x ← x + (sample − x)/8, in whole bytes and nanoseconds.
        let smooth = |avg: u64, sample: u64| if avg == 0 { sample } else { avg - avg / 8 + sample / 8 };
        self.bytes.store(smooth(self.bytes.load(Relaxed), len), Relaxed);
        self.ns.store(smooth(self.ns.load(Relaxed), took), Relaxed);
    }
}

/// Format: room in [`KIND_COSTS`] for [`AgentMsg`]'s variants by their index on the wire (its table,
/// `crate::wire::agent_classes`, has 70-some; a test holds it within), a kind past it sharing the rest's.
const KINDS: usize = 128;

/// What reading a runner's message of each kind is expected to take, as the hub's loop reads frames
/// ([`read_frame`]) and channel items ([`read_item`]): an [`Expect`] for each of [`AgentMsg`]'s variants (goal
/// 9, plan 01a1a87b2cec step 3). One for all judged an agent session's update, whose JSON costs much more a
/// byte, by the many messages that carry bytes (terminal output, streams): mid-sized updates were read on
/// the shard, up to 2.4 ms each in the lab's g6c2.
static KIND_COSTS: [Expect; KINDS] = [const { Expect::nothing_yet() }; KINDS];

/// What [`KIND_COSTS`] has no room for: kinds past it, and what doesn't say its kind.
static REST_COSTS: Expect = Expect::nothing_yet();

/// The kind of runner message `data` holds: its variant's index on the wire, postcard's first varint (up to
/// a `u32`'s five bytes); [`KINDS`] − 1 for one past the room or that can't be read.
fn kind_of(data: &[u8]) -> usize {
    let mut index: u64 = 0;
    for (i, b) in data.iter().take(5).enumerate() {
        index |= u64::from(b & 0x7f) << (7 * i);
        if b & 0x80 == 0 {
            return usize::try_from(index).map_or(KINDS - 1, |k| k.min(KINDS - 1));
        }
    }
    KINDS - 1
}

/// Format: the kinds of agent session update whose reads are expected apart ([`UPDATE_COSTS`]), by the tag each is
/// written with (`crate::harness::core::Update`'s `t`, in its order; a test holds them to what it writes).
const UPDATE_TAGS: [&str; 11] = ["open", "closed", "state", "meta", "events", "changes", "stream", "reply", "inbox", "reset", "changes_patch"];

/// What reading an agent session's update of each kind is expected to take: an [`Expect`] for each of [`UPDATE_TAGS`]
/// (goal 9, plan 01a1a87b2cec step 3), as [`KIND_COSTS`] keeps one for each kind of message. Their JSON costs a byte
/// far apart: a change's hunks and a session's state are many short fields, an event's reply one long text. Under one
/// expectation for all of them, a long change was read on the shard at the rate the long replies before it had set.
static UPDATE_COSTS: [Expect; UPDATE_TAGS.len()] = [const { Expect::nothing_yet() }; UPDATE_TAGS.len()];

/// The kind of agent session update `json` is: its tag's place in [`UPDATE_TAGS`], where the tag comes first, as every
/// writer of an update puts it (serde's tagged serializer: `crate::harness::core::Update`); none for an update that says
/// it otherwise, or a kind this build doesn't know.
fn update_kind(json: &[u8]) -> Option<usize> {
    let rest = json.strip_prefix(br#"{"t":""#)?;
    let tag = rest.get(..rest.iter().position(|b| *b == b'"')?)?;
    UPDATE_TAGS.iter().position(|t| t.as_bytes() == tag)
}

/// What reading `data`, a runner's message, is expected to take, by its kind ([`KIND_COSTS`]); an agent session's
/// update by its own kind ([`UPDATE_COSTS`]).
fn costs_of(data: &[u8]) -> &'static Expect {
    if let Some(update) = crate::proto::harness_at(data)
        && let Some(costs) = update_kind(update.json).and_then(|k| UPDATE_COSTS.get(k))
    {
        return costs;
    }
    KIND_COSTS.get(kind_of(data)).unwrap_or(&REST_COSTS)
}

/// What reading runner frame `data` is expected to hold the shard for, nanoseconds: its own read ([`read_frame`]) and, a
/// channel item's, the read of the message it carries ([`read_item`]), each where it's expected to be read on the shard,
/// and nothing for one expected to be read off it. A loop gives its shard up before a frame whose read would take its turn
/// past its quantum (`crate::rt::Turns::before`; goal 9, plan 01a1a87b2cec step 3): a turn of short pieces took a long
/// read last, past the quantum, its pieces so far having said nothing of it.
pub fn expected_ns(data: &[u8]) -> u64 {
    expected_by(data, costs_of)
}

/// [`expected_ns`] by the expectations `costs` gives each message.
fn expected_by<'a>(data: &[u8], costs: impl Fn(&[u8]) -> &'a Expect) -> u64 {
    let on_shard = |data: &[u8]| {
        let ns = costs(data).expected(u64::try_from(data.len()).unwrap_or(u64::MAX));
        if ns > hyper_rt::slow::SLOW_POLL_NS { 0 } else { ns }
    };
    let carried = crate::proto::item_at(data).map_or(0, |item| on_shard(item.body));
    on_shard(data).saturating_add(carried)
}

/// Reads runner frame `data` (postcard) and what its message carries (JSON, [`payload_of`]); the
/// latter only for a runner whose Hello has come (`hello`), or for a message that may come before it
/// (`Hello`, `Instance`, `Machine`). On the shard while that's expected ([`Expect`]) to take less than a
/// slow poll (`hyper_rt::slow::SLOW_POLL_NS`), else on the compute pool, the caller waiting either way, so
/// its messages are still taken in order (goal 9, plan 01a1a87b2cec step 3: the hub's net loop read
/// runners' frames on its shard, holding it up to 76 ms).
pub async fn read_frame(data: Vec<u8>, hello: bool) -> Frame {
    read_frame_by(costs_of(&data), data, hello).await
}

/// A runner's channel item (`crate::wire::channel::Frame::Item`), as [`read_item`] read it.
pub enum Item {
    /// The message it carries isn't one this build reads.
    Unreadable(crate::proto::Unreadable),
    /// The message it carries, what that carries read ([`payload_of`]), and the label of its kind for the
    /// shards' table of what held them (`crate::wire::agent_label`).
    Read { label: &'static hyper_rt::slow::Label, carried: Result<Carried<AgentMsg, FromRunner>, Box<Unreadable>> },
}

/// Reads the message channel item `body` carries, and what that carries, as [`read_frame`] reads a frame: on
/// the shard while that's expected to take less than a slow poll, else on the compute pool, the caller
/// waiting either way, so the items are still taken in order (goal 9, plan 01a1a87b2cec step 3: the channel
/// carries most of what a runner sends, its sessions' updates among it, which the hub read on its shard
/// whatever their length; the lab's s7c had its loop's polls under that label held up to 28 ms). An item
/// comes after its runner's Hello.
pub async fn read_item(body: Vec<u8>) -> Item {
    match read_frame_by(costs_of(&body), body, true).await {
        Frame::Read { label, carried } => Item::Read { label, carried },
        Frame::Unreadable(e) => Item::Unreadable(e),
        // Read as after its Hello, so none comes before it; said as what it would be.
        Frame::BeforeHello { what } => Item::Unreadable(crate::proto::Unreadable::Malformed { variant: None, why: format!("{what}, before its runner's Hello") }),
    }
}

/// [`read_frame`] by what `expect` learned. A read is timed where it runs, so one off the shard teaches
/// it too: else one slow sample (a preempted read) could send every frame of a runner sending only long
/// ones off the shard for good, none read on it to bring the expectation back down.
async fn read_frame_by(expect: &Expect, data: Vec<u8>, hello: bool) -> Frame {
    let len = u64::try_from(data.len()).unwrap_or(u64::MAX);
    let (frame, took) = if expect.expected(len) > hyper_rt::slow::SLOW_POLL_NS {
        match crate::rt::compute(move || timed(|| read_now(data, hello))).await {
            Ok(read) => read,
            Err(e) => return Frame::Unreadable(crate::proto::Unreadable::Malformed { variant: None, why: format!("it couldn't be read off the shard: {e:#}") }),
        }
    } else {
        timed(|| read_now(data, hello))
    };
    expect.learn(len, took);
    frame
}

/// `read()`'s frame, and how long it took, nanoseconds.
fn timed(read: impl FnOnce() -> Frame) -> (Frame, u64) {
    let began = Instant::now();
    let frame = read();
    (frame, u64::try_from(began.elapsed().as_nanos()).unwrap_or(u64::MAX))
}

/// [`read_frame`]'s work, where it runs: a channel item's body and an update's JSON kept in the frame's own buffer
/// (`crate::proto::read_agent_owned`; goal 9, plan 01a1a87b2cec step 5).
fn read_now(data: Vec<u8>, hello: bool) -> Frame {
    match crate::proto::read_agent_owned(data) {
        Err(e) => Frame::Unreadable(e),
        Ok(msg) if !hello && !matches!(msg, AgentMsg::Hello(_) | AgentMsg::Instance(_) | AgentMsg::Machine { .. }) => Frame::BeforeHello { what: super::agent_name(&msg).to_owned() },
        Ok(msg) => Frame::Read { label: super::agent_label(&msg), carried: payload_of(msg) },
    }
}

/// An answer by `req` to something the server asked a runner ([`AgentMsg::Answer`],
/// `AiTranscript`, `AiView`, `HarnessTaskOutput`, `HarnessHistory`, `DirList`). It's of the type
/// its question asks for, so it's read once it's matched to its question (`crate::hub`'s asks), by
/// [`payload`] as that type.
pub struct Answer {
    pub of: &'static str,
    pub req: u64,
    pub json: Option<String>,
}

/// What a runner's messages carry, as the server reads it: each of [`AgentMsg`]'s variants that
/// carries JSON, with its payload's type in place of the JSON.
pub enum FromRunner {
    /// `AiSessions`: the AI coding sessions running on its machine.
    AiSessions(Vec<crate::aisessions::AiSession>),
    /// `AiTranscript`, `AiView`, `HarnessTaskOutput`, `HarnessHistory`, `DirList`, `Answer`.
    Answer(Answer),
    /// `Harness`: one update of agent session `session`; and how long before its frame was made its runner
    /// took it in, where its runner said (`crate::wire::stamp`; goal 10, plan 01a1a87b2dae step 5).
    Harness { session: String, update: Box<crate::harness::core::Update>, taken: Option<Duration> },
    HarnessInventory(Box<crate::harness::host::Inventory>),
    Sync { msg: Box<crate::sync::wire::Msg>, raft: Vec<u8> },
    FrontDoorStatus(Box<crate::frontdoor::Status>),
    JobEvent { job: String, event: Box<crate::proto::JobEvent> },
    Facts(Box<kiln_ci::facts::RunnerFacts>),
    ReachHello(crate::reach::relay::Announce),
    /// `ReachAsk`: a question for the server; and the question as it came, which an agent's act is
    /// checked against (signed over its bytes, docs/ci.md W27-R08).
    ReachAsk { req: u64, ask: Box<crate::reach::Question>, raw: Value },
    Convo { msg: Box<crate::convo::feed::Msg>, data: Vec<u8> },
    Power(crate::os::battery::Battery),
    HarnessFileData { req: u64, answer: crate::harness::files::Answer, data: Vec<u8> },
    /// `StreamEvent`: what stream `stream` says (opened, navigated, loaded, focus, dialog, closed,
    /// error).
    StreamEvent { stream: String, event: Value },
    Executor(crate::runner::pool::Said),
    Undeployed(kiln_ci::wire::Undeployed),
    Cluster { msg: Box<crate::store::learner::Msg>, raft: Vec<u8> },
    /// `WorkState`: what it holds warm, for placement (docs/ci.md W24-R18).
    WorkState(Box<kiln_ci::work::slots::WorkState>),
    /// `AgentLimit`: the most agent sessions its machine has at work at once, and where that comes
    /// from (docs/agents.md, "How many agents a machine runs").
    AgentLimit(crate::harness::limit::Held),
    /// `Identity`: a certificate asked for, for a key the runner process made and keeps (docs/ci.md
    /// W27-R01).
    Identity(crate::proto::IdentityAsk),
    /// `Observe`: its replica's word in observe's group (docs/observe.md §5), and what it carries.
    Observe { msg: Box<crate::observe::wire::Msg>, bytes: Vec<u8> },
    /// `HeadroomStatus`: how its machine's headroom does (docs/headroom.md), kept and shown as it
    /// came: of it, the server reads only the fingerprint of the settings in use.
    HeadroomStatus(Value),
    /// `LegResults`: a leg's records, signed as the leg at its end (docs/ci.md W27-R05): Sigstore
    /// bundles, each checked as it's taken.
    LegResults { job: String, bundles: Vec<Value> },
    /// `LegSignature`: the provenance kiln wrote of what a leg built, signed as the leg (a Sigstore
    /// bundle), or why not (docs/ci.md W27-R06).
    LegSignature { job: String, nonce: String, bundle: Option<Value>, error: Option<String> },
    /// `Custody`: a custody record or delegation a runner process or a session it holds signed, or
    /// carried (a Sigstore bundle, docs/ci.md W27-R08).
    Custody(Value),
}

impl Carries for AgentMsg {
    type Read = FromRunner;

    fn read(self) -> Result<Carried<Self, FromRunner>, Box<Unreadable>> {
        const W: Wire = Wire::RunnerPayload;
        let read = match self {
            AgentMsg::AiSessions { json } => FromRunner::AiSessions(payload_owned(W, "AiSessions", json)?),
            AgentMsg::AiTranscript { req, json } => FromRunner::Answer(Answer { of: "AiTranscript", req, json }),
            // Its runner's stamp, off its end first: the update is read as one without it (goal 10, plan
            // 01a1a87b2dae step 5).
            AgentMsg::Harness { session, mut json } => {
                let taken = super::stamp::take(&mut json);
                match payload_owned(W, "Harness", json) {
                    Ok(update) => FromRunner::Harness { session, update, taken },
                    Err(e) => return Err(e.about(&session)),
                }
            }
            AgentMsg::HarnessInventory { json } => FromRunner::HarnessInventory(payload_owned(W, "HarnessInventory", json)?),
            AgentMsg::AiView { req, json } => FromRunner::Answer(Answer { of: "AiView", req, json }),
            AgentMsg::HarnessTaskOutput { req, json } => FromRunner::Answer(Answer { of: "HarnessTaskOutput", req, json }),
            AgentMsg::DirList { req, json } => FromRunner::Answer(Answer { of: "DirList", req, json: Some(json) }),
            AgentMsg::HarnessHistory { req, json } => FromRunner::Answer(Answer { of: "HarnessHistory", req, json }),
            AgentMsg::Sync { json, raft } => FromRunner::Sync { msg: payload_owned(W, "Sync", json)?, raft },
            AgentMsg::FrontDoorStatus { json } => FromRunner::FrontDoorStatus(payload_owned(W, "FrontDoorStatus", json)?),
            AgentMsg::JobEvent { job, json } => FromRunner::JobEvent { job, event: payload_owned(W, "JobEvent", json)? },
            AgentMsg::Facts { json } => FromRunner::Facts(payload_owned(W, "Facts", json)?),
            AgentMsg::ReachHello { json } => FromRunner::ReachHello(payload_owned(W, "ReachHello", json)?),
            AgentMsg::ReachAsk { req, json } => {
                let ask = payload(W, "ReachAsk", &json).map_err(|e| e.waits(Waits::Asking(req)))?;
                FromRunner::ReachAsk { req, ask, raw: serde_json::from_str(&json).unwrap_or_default() }
            }
            AgentMsg::Convo { json, data } => FromRunner::Convo { msg: payload_owned(W, "Convo", json)?, data },
            AgentMsg::Power { json } => FromRunner::Power(payload_owned(W, "Power", json)?),
            AgentMsg::Answer { req, json } => FromRunner::Answer(Answer { of: "Answer", req, json }),
            AgentMsg::HarnessFileData { req, json, data } => FromRunner::HarnessFileData { req, answer: payload_owned(W, "HarnessFileData", json).map_err(|e| e.waits(Waits::Answering(req)))?, data },
            AgentMsg::StreamEvent { stream, json } => FromRunner::StreamEvent { stream, event: payload_owned(W, "StreamEvent", json)? },
            AgentMsg::Executor { json } => FromRunner::Executor(payload_owned(W, "Executor", json)?),
            AgentMsg::Undeployed { json } => FromRunner::Undeployed(payload_owned(W, "Undeployed", json)?),
            AgentMsg::Cluster { json, raft } => FromRunner::Cluster { msg: payload_owned(W, "Cluster", json)?, raft },
            AgentMsg::WorkState { json } => FromRunner::WorkState(payload_owned(W, "WorkState", json)?),
            AgentMsg::AgentLimit { json } => FromRunner::AgentLimit(payload_owned(W, "AgentLimit", json)?),
            AgentMsg::Identity { json } => FromRunner::Identity(payload_owned(W, "Identity", json)?),
            AgentMsg::Observe { json, bytes } => FromRunner::Observe { msg: payload_owned(W, "Observe", json)?, bytes },
            AgentMsg::HeadroomStatus { json } => FromRunner::HeadroomStatus(payload_owned(W, "HeadroomStatus", json)?),
            AgentMsg::LegResults { job, bundles } => FromRunner::LegResults { job, bundles: payload_owned(W, "LegResults", bundles)? },
            AgentMsg::LegSignature { job, nonce, bundle, error } => {
                let bundle = match bundle {
                    Some(b) => Some(payload_owned(W, "LegSignature", b)?),
                    None => None,
                };
                FromRunner::LegSignature { job, nonce, bundle, error }
            }
            AgentMsg::Custody { json } => FromRunner::Custody(payload_owned(W, "Custody", json)?),
            m @ (AgentMsg::Hello(_)
            | AgentMsg::Heartbeat { .. }
            | AgentMsg::Sample(_)
            | AgentMsg::JobStarted { .. }
            | AgentMsg::StepStarted { .. }
            | AgentMsg::StepFinished { .. }
            | AgentMsg::JobFinished(_)
            | AgentMsg::JobSample { .. }
            | AgentMsg::Reject { .. }
            | AgentMsg::TermOutput { .. }
            | AgentMsg::TermExit { .. }
            | AgentMsg::TermList { .. }
            | AgentMsg::TermReplay { .. }
            | AgentMsg::Instance(_)
            | AgentMsg::Machine { .. }
            | AgentMsg::Build { .. }
            | AgentMsg::UpgradeLog { .. }
            | AgentMsg::UpgradeDone { .. }
            | AgentMsg::Reach { .. }
            | AgentMsg::UpdatePrefs { .. }
            | AgentMsg::StreamFrame { .. }
            | AgentMsg::PortOpened { .. }
            | AgentMsg::PortData { .. }
            | AgentMsg::PortAck { .. }
            | AgentMsg::PortClose { .. }
            | AgentMsg::Caps { .. }
            | AgentMsg::Wire(_)
            | AgentMsg::NetDrops { .. }
            | AgentMsg::HarnessHandOver { .. }
            | AgentMsg::HarnessAdopted { .. }
            | AgentMsg::Unread { .. }
            | AgentMsg::LinkUnread { .. }) => return Ok(Carried::Nothing(m)),
        };
        Ok(Carried::Read(read))
    }
}

/// What the server's messages carry, as a runner reads it: each of [`ServerMsg`]'s variants that
/// carries JSON, with its payload's type in place of the JSON.
pub enum FromServer {
    HarnessOpen { session: String, spec: Box<crate::harness::session::Spec> },
    HarnessCmd { session: String, command: Box<crate::harness::model::Command> },
    /// `HarnessSend`: a message for agent session `session`'s inbox, for its epoch `epoch`.
    HarnessSend { session: String, epoch: u64, command: Box<crate::harness::model::Command> },
    Sync { msg: Box<crate::sync::wire::Msg>, raft: Vec<u8> },
    FrontDoor(Box<crate::frontdoor::Push>),
    AssignV2(Box<kiln_ci::wire::AssignmentV2>),
    SecretsFor { job: String, secrets: Vec<crate::proto::SecretFor> },
    /// `ReachAnswer`: the server's answer to a runner's `ReachAsk`.
    ReachAnswer { req: u64, answer: Value },
    Convo { msg: Box<crate::convo::feed::Msg>, data: Vec<u8> },
    StreamOpen { stream: String, session: String, spec: Box<crate::browser::stream::Spec> },
    /// `StreamControl`: what a viewer asks of stream `stream` (`crate::browser::stream::Stream::control`).
    StreamControl { stream: String, session: String, control: Value },
    StreamInput { stream: String, session: String, input: Vec<crate::browser::stream::Input> },
    /// `Preview`: what a person opens live from agent session `session` (`{"op": "look"}`).
    Preview { req: u64, session: String, ask: Value },
    Workspace { req: u64, session: String, ask: Box<crate::workspace::Ask> },
    Executor(crate::runner::pool::Ask),
    Undeploy(kiln_ci::wire::Undeploy),
    Cluster { msg: Box<crate::store::learner::Msg>, raft: Vec<u8> },
    /// `AgentLimit`: what a person set from the server for the most agent sessions this machine
    /// has at work at once (none: its config's holds).
    AgentLimit(crate::harness::limit::Asked),
    /// `Identity`: the certificate for an identity it asked for, or why there's none (docs/ci.md
    /// W27-R01).
    Identity(crate::proto::IdentityGiven),
    /// `Observe`: the server's word to its replica in observe's group, and what it carries.
    Observe { msg: Box<crate::observe::wire::Msg>, bytes: Vec<u8> },
    /// `Headroom`: this machine's headroom settings (docs/headroom.md).
    Headroom(Box<crate::headroom::Push>),
    /// `Custody`: work kiln's server handed one of the runner's agent sessions, `{session, hand_on}`
    /// (docs/ci.md W27-R08).
    Custody(Value),
}

impl Carries for ServerMsg {
    type Read = FromServer;

    fn read(self) -> Result<Carried<Self, FromServer>, Box<Unreadable>> {
        const W: Wire = Wire::ServerPayload;
        let read = match self {
            ServerMsg::HarnessOpen { session, json } => match payload(W, "HarnessOpen", &json) {
                Ok(spec) => FromServer::HarnessOpen { session, spec },
                Err(e) => return Err(e.about(&session).waits(Waits::Opening(session))),
            },
            ServerMsg::HarnessCmd { session, json } => match payload(W, "HarnessCmd", &json) {
                Ok(command) => FromServer::HarnessCmd { session, command },
                Err(e) => return Err(e.about(&session)),
            },
            ServerMsg::HarnessSend { session, epoch, json } => match payload(W, "HarnessSend", &json) {
                Ok(command) => FromServer::HarnessSend { session, epoch, command },
                // Its id, when that much of it can be read: the session's inbox answers it.
                Err(e) => match message_of(&json) {
                    Some(message) => return Err(e.about(&session).waits(Waits::Sending { session, message, epoch })),
                    None => return Err(e.about(&session)),
                },
            },
            ServerMsg::Sync { json, raft } => FromServer::Sync { msg: payload(W, "Sync", &json)?, raft },
            ServerMsg::FrontDoor { json } => FromServer::FrontDoor(payload(W, "FrontDoor", &json)?),
            ServerMsg::AssignV2 { json } => FromServer::AssignV2(payload(W, "AssignV2", &json)?),
            ServerMsg::SecretsFor { job, json } => FromServer::SecretsFor { job, secrets: payload(W, "SecretsFor", &json)? },
            ServerMsg::ReachAnswer { req, json } => FromServer::ReachAnswer { req, answer: payload(W, "ReachAnswer", &json)? },
            ServerMsg::Convo { json, data } => FromServer::Convo { msg: payload(W, "Convo", &json)?, data },
            ServerMsg::StreamOpen { stream, session, json } => match payload(W, "StreamOpen", &json) {
                Ok(spec) => FromServer::StreamOpen { stream, session, spec },
                Err(e) => return Err(e.about(&session).waits(Waits::Stream(stream))),
            },
            ServerMsg::StreamControl { stream, session, json } => match payload(W, "StreamControl", &json) {
                Ok(control) => FromServer::StreamControl { stream, session, control },
                Err(e) => return Err(e.about(&session)),
            },
            ServerMsg::StreamInput { stream, session, json } => match payload(W, "StreamInput", &json) {
                Ok(input) => FromServer::StreamInput { stream, session, input },
                Err(e) => return Err(e.about(&session)),
            },
            ServerMsg::Preview { req, session, json } => match payload(W, "Preview", &json) {
                Ok(ask) => FromServer::Preview { req, session, ask },
                Err(e) => return Err(e.about(&session).waits(Waits::Asking(req))),
            },
            ServerMsg::Workspace { req, session, json } => match payload(W, "Workspace", &json) {
                Ok(ask) => FromServer::Workspace { req, session, ask },
                Err(e) => return Err(e.about(&session).waits(Waits::Asking(req))),
            },
            ServerMsg::Executor { json } => FromServer::Executor(payload(W, "Executor", &json)?),
            ServerMsg::Undeploy { json } => FromServer::Undeploy(payload(W, "Undeploy", &json)?),
            ServerMsg::Cluster { json, raft } => FromServer::Cluster { msg: payload(W, "Cluster", &json)?, raft },
            ServerMsg::AgentLimit { json } => FromServer::AgentLimit(payload(W, "AgentLimit", &json)?),
            ServerMsg::Identity { json } => FromServer::Identity(payload(W, "Identity", &json)?),
            ServerMsg::Observe { json, bytes } => FromServer::Observe { msg: payload(W, "Observe", &json)?, bytes },
            ServerMsg::Headroom { json } => FromServer::Headroom(payload(W, "Headroom", &json)?),
            ServerMsg::Custody { json } => FromServer::Custody(payload(W, "Custody", &json)?),
            m @ (ServerMsg::Welcome { .. }
            | ServerMsg::Refused { .. }
            | ServerMsg::Assign(_)
            | ServerMsg::Cancel { .. }
            | ServerMsg::Drain
            | ServerMsg::TermOpen { .. }
            | ServerMsg::TermInput { .. }
            | ServerMsg::TermResize { .. }
            | ServerMsg::TermClose { .. }
            | ServerMsg::TermAttach { .. }
            | ServerMsg::AiTranscript { .. }
            | ServerMsg::HarnessClose { .. }
            | ServerMsg::AiView { .. }
            | ServerMsg::HarnessFile { .. }
            | ServerMsg::Retire { .. }
            | ServerMsg::Upgrade { .. }
            | ServerMsg::HarnessTaskOutput { .. }
            | ServerMsg::DirList { .. }
            | ServerMsg::HarnessHistory { .. }
            | ServerMsg::JobTerminalOpen { .. }
            | ServerMsg::KeepRelease { .. }
            | ServerMsg::ProbeFacts
            | ServerMsg::Reach { .. }
            | ServerMsg::KeepExtend { .. }
            | ServerMsg::JobRerunStep { .. }
            | ServerMsg::JobExec { .. }
            | ServerMsg::HarnessPackage { .. }
            | ServerMsg::HarnessRelease { .. }
            | ServerMsg::UpdateAvailable { .. }
            | ServerMsg::Diagnose { .. }
            | ServerMsg::Repair { .. }
            | ServerMsg::HarnessFileRead { .. }
            | ServerMsg::StreamClose { .. }
            | ServerMsg::PortOpen { .. }
            | ServerMsg::PortData { .. }
            | ServerMsg::PortAck { .. }
            | ServerMsg::PortClose { .. }
            | ServerMsg::Received { .. }
            | ServerMsg::Caps { .. }
            | ServerMsg::Wire(_)
            | ServerMsg::Kept { .. }
            | ServerMsg::Unread { .. }
            | ServerMsg::ReachRefused { .. }
            | ServerMsg::Freeze { .. }
            | ServerMsg::SignAsLeg { .. }) => return Ok(Carried::Nothing(m)),
        };
        Ok(Carried::Read(read))
    }
}

/// How a `HarnessCmd` the server sends rides the channel (`crate::wire::command_class`), from its
/// JSON: the server's own, read here like every payload. One with a message's id is refused at once
/// while its runner is away, never parked (CP-T3.2); one it can't read parks.
pub fn command_class(json: &str) -> super::Class {
    match payload::<crate::harness::model::Command>(Wire::ServerPayload, "HarnessCmd", json) {
        Ok(c) if crate::harness::registry::message_id(&c).is_some() => super::Class::Command(super::Away::Refuse),
        Ok(c) => super::command_class(&c),
        Err(_) => super::Class::Command(super::Away::Park),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::Up;
    use crate::wire::unread::Page;
    use kiln_seams::clock::SimClock;

    /// A variant that carries JSON, for the tests: how to make it around its JSON, a payload it
    /// takes, and one of a kind it doesn't know, for a payload tagged by its kind.
    struct Case<M> {
        of: &'static str,
        make: fn(String) -> M,
        good: String,
        unknown: Option<&'static str>,
        /// It takes any JSON (`Value`): only what isn't JSON is refused.
        any: bool,
    }

    impl<M> Case<M> {
        /// What it refuses, and as what: JSON cut short, JSON of the wrong type, a kind it doesn't
        /// know.
        fn bad(&self) -> Vec<(String, Code)> {
            let mut bad = vec![("{".to_owned(), Code::Malformed)];
            if !self.any {
                bad.push(("\"x\"".to_owned(), Code::Malformed));
            }
            bad.extend(self.unknown.map(|u| (u.to_owned(), Code::Unknown)));
            bad
        }
    }

    fn case<M>(of: &'static str, make: fn(String) -> M, good: &impl Serialize, unknown: Option<&'static str>) -> Case<M> {
        Case { of, make, good: serde_json::to_string(good).unwrap_or_default(), unknown, any: false }
    }

    /// A payload of any JSON (`Value`).
    fn any<M>(of: &'static str, make: fn(String) -> M, good: &str) -> Case<M> {
        Case { of, make, good: good.to_owned(), unknown: None, any: true }
    }

    /// The runner's answers to the server's questions: read once matched to their question.
    const ANSWERS: [&str; 6] = ["AiTranscript", "AiView", "HarnessTaskOutput", "DirList", "HarnessHistory", "Answer"];

    /// Each of `AgentMsg`'s variants that carries JSON but the answers ([`ANSWERS`]).
    fn runner_cases() -> Vec<Case<AgentMsg>> {
        let event = crate::proto::JobEvent { seq: Some(1), event: kiln_ci::wire::JobEventV2::StepStarted { idx: 0, name: "build".into(), kind: "run".into(), action: None } };
        let battery = crate::os::battery::Battery { percent: 50, ac: true, charging: false, minutes: None };
        vec![
            case("AiSessions", |json| AgentMsg::AiSessions { json }, &Vec::<crate::aisessions::AiSession>::new(), None),
            case("Harness", |json| AgentMsg::Harness { session: "s1".into(), json }, &crate::harness::core::Update::Closed, Some(r#"{"t":"teleport"}"#)),
            case("HarnessInventory", |json| AgentMsg::HarnessInventory { json }, &crate::harness::host::Inventory::default(), None),
            case("Sync", |json| AgentMsg::Sync { json, raft: Vec::new() }, &crate::sync::wire::Msg::Release, Some(r#"{"kind":"teleport"}"#)),
            case("FrontDoorStatus", |json| AgentMsg::FrontDoorStatus { json }, &crate::frontdoor::Status::default(), None),
            case("JobEvent", |json| AgentMsg::JobEvent { job: "leg-1".into(), json }, &event, Some(r#"{"seq":2,"t":"teleport"}"#)),
            case("Facts", |json| AgentMsg::Facts { json }, &kiln_ci::facts::RunnerFacts::default(), None),
            case("ReachHello", |json| AgentMsg::ReachHello { json }, &crate::reach::relay::Announce::default(), None),
            case("ReachAsk", |json| AgentMsg::ReachAsk { req: 3, json }, &crate::reach::Question::Directory, Some(r#"{"op":"teleport"}"#)),
            case("Convo", |json| AgentMsg::Convo { json, data: Vec::new() }, &crate::convo::feed::Msg::Restore { store: "st".into() }, Some(r#"{"t":"teleport"}"#)),
            case("Power", |json| AgentMsg::Power { json }, &battery, None),
            case("HarnessFileData", |json| AgentMsg::HarnessFileData { req: 4, json, data: Vec::new() }, &crate::harness::files::Answer::default(), None),
            any("StreamEvent", |json| AgentMsg::StreamEvent { stream: "st1".into(), json }, r#"{"opened":{}}"#),
            case("Executor", |json| AgentMsg::Executor { json }, &crate::runner::pool::Said::default(), None),
            case("Undeployed", |json| AgentMsg::Undeployed { json }, &kiln_ci::wire::Undeployed::default(), None),
            case("Cluster", |json| AgentMsg::Cluster { json, raft: Vec::new() }, &crate::store::learner::Msg::Leave, Some(r#"{"t":"teleport"}"#)),
            case("WorkState", |json| AgentMsg::WorkState { json }, &kiln_ci::work::slots::WorkState::default(), None),
            case("AgentLimit", |json| AgentMsg::AgentLimit { json }, &crate::harness::limit::Held { max_sessions: 8, from: crate::harness::limit::Source::Config, config: Some(8) }, None),
            case("Identity", |json| AgentMsg::Identity { json }, &crate::proto::IdentityAsk { kind: "runner".into(), csr: "csr".into(), ..Default::default() }, None),
            case("Observe", |json| AgentMsg::Observe { json, bytes: Vec::new() }, &crate::observe::wire::Msg::Release, Some(r#"{"kind":"teleport"}"#)),
            any("HeadroomStatus", |json| AgentMsg::HeadroomStatus { json }, r#"{"settings":"f1","enabled":true}"#),
        ]
    }

    /// Each of `ServerMsg`'s variants that carries JSON.
    fn server_cases() -> Vec<Case<ServerMsg>> {
        let spec = crate::harness::session::Spec { kind: "claude".into(), ..Default::default() };
        let send = r#"{"type":"send","id":"m1","text":"hi"}"#;
        let mut cases = vec![
            case("HarnessOpen", |json| ServerMsg::HarnessOpen { session: "s1".into(), json }, &spec, None),
            case("HarnessCmd", |json| ServerMsg::HarnessCmd { session: "s1".into(), json }, &crate::harness::model::Command::Interrupt, Some(r#"{"type":"teleport"}"#)),
            Case { of: "HarnessSend", make: |json| ServerMsg::HarnessSend { session: "s1".into(), epoch: 1, json }, good: send.into(), unknown: Some(r#"{"type":"teleport","id":"m2"}"#), any: false },
            case("Sync", |json| ServerMsg::Sync { json, raft: Vec::new() }, &crate::sync::wire::Msg::Release, Some(r#"{"kind":"teleport"}"#)),
            case("FrontDoor", |json| ServerMsg::FrontDoor { json }, &crate::frontdoor::Push::default(), None),
            case("AssignV2", |json| ServerMsg::AssignV2 { json }, &kiln_ci::wire::AssignmentV2::default(), None),
            case("SecretsFor", |json| ServerMsg::SecretsFor { job: "leg-1".into(), json }, &Vec::<crate::proto::SecretFor>::new(), None),
            any("ReachAnswer", |json| ServerMsg::ReachAnswer { req: 5, json }, r#"{"runners":[]}"#),
            case("Convo", |json| ServerMsg::Convo { json, data: Vec::new() }, &crate::convo::feed::Msg::Restore { store: "st".into() }, Some(r#"{"t":"teleport"}"#)),
            any("StreamControl", |json| ServerMsg::StreamControl { stream: "st1".into(), session: "s1".into(), json }, r#"{"resize":{"width":390}}"#),
            case("StreamInput", |json| ServerMsg::StreamInput { stream: "st1".into(), session: "s1".into(), json }, &Vec::<crate::browser::stream::Input>::new(), Some(r#"[{"t":"teleport"}]"#)),
            any("Preview", |json| ServerMsg::Preview { req: 6, session: "s1".into(), json }, r#"{"op":"look"}"#),
            Case { of: "Workspace", make: |json| ServerMsg::Workspace { req: 7, session: "s1".into(), json }, good: r#"{"op":"git"}"#.into(), unknown: Some(r#"{"op":"teleport"}"#), any: false },
            case("Executor", |json| ServerMsg::Executor { json }, &crate::runner::pool::Ask::default(), None),
            case("Undeploy", |json| ServerMsg::Undeploy { json }, &kiln_ci::wire::Undeploy::default(), None),
            case("Cluster", |json| ServerMsg::Cluster { json, raft: Vec::new() }, &crate::store::learner::Msg::Leave, Some(r#"{"t":"teleport"}"#)),
            case("AgentLimit", |json| ServerMsg::AgentLimit { json }, &crate::harness::limit::Asked { max_sessions: Some(8) }, None),
            case("Identity", |json| ServerMsg::Identity { json }, &crate::proto::IdentityGiven { kind: "runner".into(), uri: "kiln://runner/m1".into(), ..Default::default() }, None),
            case("Observe", |json| ServerMsg::Observe { json, bytes: Vec::new() }, &crate::observe::wire::Msg::Release, Some(r#"{"kind":"teleport"}"#)),
            case("Headroom", |json| ServerMsg::Headroom { json }, &crate::headroom::Push::default(), None),
        ];
        cases.push(Case { of: "StreamOpen", make: |json| ServerMsg::StreamOpen { stream: "st1".into(), session: "s1".into(), json }, good: r#"{"url":"https://example.com"}"#.into(), unknown: None, any: false });
        cases
    }

    /// Whether `msg` came through as what it carries, read.
    fn read<M: Carries>(msg: M) -> Result<bool, Box<Unreadable>> {
        payload_of(msg).map(|c| matches!(c, Carried::Read(_)))
    }

    /// Every variant that carries JSON, on both sides: what it takes it reads into its type; what
    /// isn't JSON, or isn't of its type, is `malformed`; a kind it doesn't know is `unknown`, saying
    /// the kind. Each refusal says which variant it was.
    #[test]
    fn every_payload_is_read_into_its_type_or_refused_saying_why() -> anyhow::Result<()> {
        let (runner, server) = (runner_cases(), server_cases());
        // On main as the plan was made, 22 of AgentMsg's and 16 of ServerMsg's, T3's HarnessSend
        // since; W24's WorkState; a machine's agent limit, AgentLimit; W27's identities and
        // observe's group, Identity and Observe, each way; and headroom's, HeadroomStatus and
        // Headroom.
        anyhow::ensure!(runner.len() + ANSWERS.len() == 27 && server.len() == 21, "{} and {} variants", runner.len() + ANSWERS.len(), server.len());
        fn check<M: Carries>(cases: Vec<Case<M>>) -> anyhow::Result<()> {
            for c in cases {
                anyhow::ensure!(read((c.make)(c.good.clone())).map_err(|e| anyhow::anyhow!("{}: {e}", c.of))?, "{} came through as it was", c.of);
                for (json, code) in c.bad() {
                    match read((c.make)(json.clone())) {
                        Err(e) => {
                            anyhow::ensure!(e.of == c.of && e.code == code && e.what.starts_with(c.of) && !e.why.is_empty(), "{}: {json} refused as {e:?}", c.of);
                            if code == Code::Unknown {
                                anyhow::ensure!(e.what.ends_with("(teleport)"), "{}: the kind it doesn't know is said: {}", c.of, e.what);
                            }
                        }
                        Ok(_) => anyhow::bail!("{}: {json} was taken", c.of),
                    }
                }
            }
            Ok(())
        }
        check(runner)?;
        check(server)?;
        // The answers come through as they are: their question says what they're read as.
        for of in ANSWERS {
            let msg = match of {
                "AiTranscript" => AgentMsg::AiTranscript { req: 1, json: Some("{".into()) },
                "AiView" => AgentMsg::AiView { req: 1, json: Some("{".into()) },
                "HarnessTaskOutput" => AgentMsg::HarnessTaskOutput { req: 1, json: Some("{".into()) },
                "DirList" => AgentMsg::DirList { req: 1, json: "{".into() },
                "HarnessHistory" => AgentMsg::HarnessHistory { req: 1, json: Some("{".into()) },
                _ => AgentMsg::Answer { req: 1, json: Some("{".into()) },
            };
            match payload_of(msg) {
                Ok(Carried::Read(FromRunner::Answer(a))) => anyhow::ensure!(a.of == of && a.req == 1 && a.json.as_deref() == Some("{"), "{of}"),
                _ => anyhow::bail!("{of} wasn't passed on as an answer"),
            }
        }
        Ok(())
    }

    /// What it says of what it couldn't read never quotes what came: a secret's value, say.
    #[test]
    fn why_never_says_what_came() {
        let e = payload::<Vec<crate::proto::SecretFor>>(Wire::ServerPayload, "SecretsFor", r#"[{"name":"token","value":17}]"#).err();
        assert!(e.as_ref().is_some_and(|e| e.code == Code::Malformed && e.why.starts_with("invalid type: integer, expected")), "{e:?}");
        let e = payload::<u32>(Wire::ServerPayload, "Test", r#""hunter2""#).err();
        assert!(e.as_ref().is_some_and(|e| !e.why.contains("hunter2") && e.why == "invalid type: string, expected u32"), "{e:?}");
        let e = payload::<u32>(Wire::ServerPayload, "Test", "12 13").err();
        assert!(e.as_ref().is_some_and(|e| e.why.starts_with("not JSON: trailing characters")), "{e:?}");
    }

    /// A field a payload has that this build doesn't know is taken: found inside a kind of a
    /// tagged enum (where serde doesn't say), each a path once per message, and empty fields left
    /// out when written back aren't taken for unknown ones.
    #[test]
    fn a_field_this_build_doesnt_know_is_taken_and_found_where_serde_cant_say() -> anyhow::Result<()> {
        use crate::harness::core::Update;
        use crate::harness::model::{Body, Event};
        let said = |id: &str, seq: u64| Event { id: id.into(), seq, at: 1_000, parent: None, by: None, body: Body::Assistant { text: "hi".into(), streaming: false } };
        let mut came = serde_json::to_value(Update::Events { events: vec![said("a", 1), said("b", 2)] })?;
        // A newer build's: a field in each event, one more in the first (an event's body is
        // flattened into it, a kind of a tagged enum), and an empty one.
        for (at, field, v) in [("/events/0", "stars", serde_json::json!(3)), ("/events/1", "stars", serde_json::json!(4)), ("/events/0", "tone", serde_json::json!("warm")), ("", "lens", Value::Null)] {
            let o = if at.is_empty() { came.as_object_mut() } else { came.pointer_mut(at).and_then(Value::as_object_mut) };
            o.ok_or_else(|| anyhow::anyhow!("no {at}"))?.insert(field.into(), v);
        }
        let before = crate::metrics::wire().ignored.get(&[Wire::TunnelDown.name()]);
        let u: Update = payload(Wire::TunnelDown, "Harness", &came.to_string()).map_err(|e| anyhow::anyhow!("{e}"))?;
        anyhow::ensure!(matches!(&u, Update::Events { events } if events.len() == 2), "{u:?}");
        let fields = unknown_fields(&came, &serde_json::to_value(&u)?);
        anyhow::ensure!(fields == ["events[].stars", "events[].tone"], "{fields:?}");
        let counted = crate::metrics::wire().ignored.get(&[Wire::TunnelDown.name()]) - before;
        anyhow::ensure!(counted == 2, "one for each field of the message: {counted}");
        Ok(())
    }

    /// Goal 10, plan 01a1a87b2dae step 5: a runner's stamp is read off the end of its update where its frame is read,
    /// and goes beside the update, which is read as one without it: the same update, with nothing in it this build
    /// doesn't know. A build from before the stamp reads the same update and counts the field as one it doesn't
    /// know. An update with no stamp reads as it always did, saying none.
    #[test]
    fn g10_s5_a_runners_stamp_is_read_off_its_update_and_goes_beside_it() -> anyhow::Result<()> {
        use crate::harness::core::Update;
        use crate::harness::model::{Body, Event};
        let u = Update::Events { events: vec![Event { id: "e1".into(), seq: 1, at: 1_000, parent: None, by: None, body: Body::Assistant { text: "hi".into(), streaming: false } }] };
        let plain = serde_json::to_string(&u)?;
        let mut stamped = plain.clone();
        crate::wire::stamp::put(&mut stamped, Duration::from_micros(700));
        let read = |json: &str| match (AgentMsg::Harness { session: "s1".into(), json: json.to_owned() }).read() {
            Ok(Carried::Read(FromRunner::Harness { session, update, taken })) => Ok((session, *update, taken)),
            _ => Err(anyhow::anyhow!("not read as a session's update: {json}")),
        };
        anyhow::ensure!(read(&stamped)? == ("s1".to_owned(), u.clone(), Some(Duration::from_micros(700))), "stamped: {stamped}");
        anyhow::ensure!(read(&plain)? == ("s1".to_owned(), u, None), "plain: {plain}");
        let mut cut = stamped.clone();
        crate::wire::stamp::take(&mut cut);
        let (known, older) = (unknown_in::<Update>(&cut), unknown_in::<Update>(&stamped));
        anyhow::ensure!(known.is_empty() && older == ["taken_us"], "unknown here: {known:?}; to a build from before it: {older:?}");
        Ok(())
    }

    /// Each wire's unknown field is said once an hour, however often it comes.
    #[test]
    fn each_unknown_field_is_said_once_an_hour() {
        let clock = SimClock::new();
        let mut ignored = Ignored::new(Arc::new(clock.clone()));
        let fields = vec!["new".to_owned(), "events[].tone".to_owned()];
        assert_eq!(ignored.note(Wire::McpReach, &fields), fields, "said the first time");
        clock.advance(Duration::from_secs(59 * 60));
        assert!(ignored.note(Wire::McpReach, &fields).is_empty(), "not again within the hour");
        let first = fields.get(..1).unwrap_or_default();
        assert_eq!(ignored.note(Wire::McpHttp, first), first.to_vec(), "another wire's are its own");
        clock.advance(Duration::from_secs(60));
        assert_eq!(ignored.note(Wire::McpReach, &fields), fields, "said again an hour on");
    }

    /// Any string at all as any payload: never a panic, always a value or a refusal saying why.
    #[test]
    fn any_string_is_read_as_a_payload_or_refused() {
        let mut x: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let (mut taken, mut refused) = (0u32, 0u32);
        fn tally<M: Carries>(msg: M, taken: &mut u32, refused: &mut u32) {
            match payload_of(msg) {
                Ok(_) => *taken += 1,
                Err(e) => {
                    assert!(!e.why.is_empty() && !e.what.is_empty(), "{e:?}");
                    *refused += 1;
                }
            }
        }
        let mut strings = |good: &str| -> Vec<String> {
            let mut out = Vec::new();
            for _ in 0..300 {
                let len = usize::try_from(next() % 48).unwrap_or(0);
                let bytes: Vec<u8> = (0..len).map(|_| (next() & 0xff) as u8).collect();
                out.push(String::from_utf8_lossy(&bytes).into_owned());
                // What it takes, cut, a byte of it changed, or something put in it.
                let mut chars: Vec<char> = good.chars().collect();
                let at = usize::try_from(next()).unwrap_or(0) % chars.len().max(1);
                match next() % 3 {
                    0 => chars.truncate(at),
                    1 => {
                        if let Some(c) = chars.get_mut(at) {
                            *c = char::from(b' ' + (next() % 95) as u8);
                        }
                    }
                    _ => chars.insert(at, ['{', '}', '[', ']', '"', ',', ':', '0', 'x'].get(usize::try_from(next() % 9).unwrap_or(0)).copied().unwrap_or('x')),
                }
                out.push(chars.into_iter().collect());
            }
            out
        };
        for c in runner_cases() {
            for s in strings(&c.good) {
                tally((c.make)(s), &mut taken, &mut refused);
            }
        }
        for c in server_cases() {
            for s in strings(&c.good) {
                tally((c.make)(s), &mut taken, &mut refused);
            }
        }
        assert!(taken > 0 && refused > 0, "{taken} taken, {refused} refused");
    }

    /// A send whose command its runner can't read, but whose id it can (agreed with T3): what waits
    /// on it is its session's inbox here, for its epoch (`Waits::Sending`), which answers it as it
    /// answers any message (`harness::host`'s tests: a repeat of one it took answered as one, one
    /// never taken refused for good). The server hears of the frame, for the session's page too.
    #[test]
    fn a_send_its_runner_cant_read_goes_to_its_sessions_inbox() -> anyhow::Result<()> {
        let unread = |json: &str| payload_of(ServerMsg::HarnessSend { session: "s1".into(), epoch: 4, json: json.into() });
        let Err(e) = unread(r#"{"type":"teleport","id":"m1","text":"hi"}"#) else { anyhow::bail!("a command this runner doesn't know was read") };
        anyhow::ensure!(matches!(&e.waits, Some(Waits::Sending { session, message, epoch: 4 }) if session == "s1" && message == "m1"), "{:?}", e.waits);
        let said = crate::agent::unread_payload(&e, Some(0));
        anyhow::ensure!(said.iter().any(|u| matches!(u, Up::Msg(AgentMsg::Unread { session: Some(s), .. }) if s == "s1")), "the server is told, for the session's page");
        // No id that can be read: nothing waits on it here.
        let Err(e) = unread(r#"{"type":"teleport"}"#) else { anyhow::bail!("a command this runner doesn't know was read") };
        anyhow::ensure!(e.waits.is_none(), "{:?}", e.waits);
        Ok(())
    }

    /// What the runner sends of a payload of the server's it couldn't read: word of it to a server
    /// that takes it, and what waits on it told why; the command it came in NACKed with the code
    /// the channel has for it.
    #[test]
    fn a_runner_says_what_it_couldnt_read_of_the_servers() -> anyhow::Result<()> {
        for c in server_cases() {
            for (json, code) in c.bad() {
                let Err(e) = payload_of((c.make)(json.clone())) else { anyhow::bail!("{}: {json} was taken", c.of) };
                anyhow::ensure!(e.code.refusal() == if code == Code::Unknown { super::super::channel::Refusal::Unsupported } else { super::super::channel::Refusal::Malformed }, "{}", c.of);
                let said = crate::agent::unread_payload(&e, Some(0));
                let told = said.iter().any(|u| matches!(u, Up::Msg(AgentMsg::Unread { wire: Wire::ServerPayload, what, code: k, .. }) if what.starts_with(c.of) && *k == code));
                anyhow::ensure!(told, "{}: {json}: the server is told", c.of);
                anyhow::ensure!(!crate::agent::unread_payload(&e, None).iter().any(|u| matches!(u, Up::Msg(AgentMsg::Unread { .. }))), "{}: a server from before isn't", c.of);
                let waited = match c.of {
                    "Preview" | "Workspace" => said.iter().any(|u| matches!(u, Up::Term(AgentMsg::Answer { json: Some(j), .. }) if j.contains("\"error\""))),
                    "StreamOpen" => said.iter().any(|u| matches!(u, Up::Stream(AgentMsg::StreamEvent { stream, json }) if stream == "st1" && json.contains("\"error\""))),
                    "HarnessOpen" => said.iter().any(|u| matches!(u, Up::Term(AgentMsg::Harness { session, .. }) if session == "s1")),
                    _ => said.len() == 1,
                };
                anyhow::ensure!(waited, "{}: what waits on it is told: {} sent", c.of, said.len());
            }
        }
        Ok(())
    }

    /// What a runner's payloads that can't be read come to on the server, for every variant that
    /// carries JSON: off the channel, `Unread` to a runner that takes it (and a question it asked
    /// answered so, the server's own asker told so); on it (a session's updates), the item's NACK.
    /// Either way counted, audited naming the runner, and kept for its page, and for the session's
    /// for a session's update.
    #[test]
    fn a_runners_payloads_that_cant_be_read_are_refused_to_it_and_noted() -> anyhow::Result<()> {
        use crate::hub::tests::{asking, asking_file, hear, hear_item, hub_with_runner, refused_rows};
        use crate::proto::{CAP_UNREAD, CAP_WIRE, LANE_CONTROL};
        use crate::wire::channel::{Frame, Refusal};
        crate::rt::block_on(async {
            let clock = SimClock::new();
            let hub = hub_with_runner(Arc::new(clock.clone()), "fedora", CAP_UNREAD | CAP_WIRE).await?;
            let mut noted = 0u64;
            let told_last = |hub: &crate::hub::Hub| hub.sent.lock().iter().rev().find_map(|(p, m)| match m {
                ServerMsg::Unread { wire, what, code, .. } if *p == 1 => Some((*wire, what.clone(), *code)),
                _ => None,
            });
            for c in runner_cases() {
                for (json, code) in c.bad() {
                    // Far enough apart that each is told (16 a second at most).
                    clock.advance(Duration::from_millis(100));
                    let (asked, file) = (asking::<Value>(&hub, 3), asking_file(&hub, 4));
                    hear(&hub, 1, LANE_CONTROL, &(c.make)(json.clone())).await?;
                    noted += 1;
                    let told = told_last(&hub);
                    anyhow::ensure!(told.as_ref().is_some_and(|(w, what, k)| *w == Wire::RunnerPayload && what.starts_with(c.of) && *k == code), "{}: {json}: told {told:?}", c.of);
                    match c.of {
                        "ReachAsk" => {
                            let answered = hub.sent.lock().iter().any(|(_, m)| matches!(m, ServerMsg::ReachAnswer { req: 3, json } if json.contains("\"error\"")));
                            anyhow::ensure!(answered, "its question answered so");
                        }
                        "HarnessFileData" => anyhow::ensure!(matches!(file.await, Ok(Err(_))), "the server's asker told so"),
                        _ => {}
                    }
                    drop(asked);
                }
            }
            // An answer that can't be read as its question asks: its asker is told why, and the
            // runner is refused it.
            clock.advance(Duration::from_millis(100));
            let asked = asking::<crate::doctor::Machine>(&hub, 9);
            hear(&hub, 1, LANE_CONTROL, &AgentMsg::Answer { req: 9, json: Some("[1, 2]".into()) }).await?;
            noted += 1;
            anyhow::ensure!(matches!(asked.await, Ok(Err(why)) if why.starts_with("invalid type")), "the asker is told why");
            anyhow::ensure!(told_last(&hub).is_some_and(|(_, what, k)| what == "Answer" && k == Code::Malformed), "the runner is told");
            // A session's update on the channel that can't be read: NACKed, never answered taken.
            let mut seq = 0;
            for (json, code) in runner_cases().into_iter().find(|c| c.of == "Harness").map(|c| c.bad()).unwrap_or_default() {
                seq += 1;
                clock.advance(Duration::from_millis(100));
                hear_item(&hub, 1, "s:s1#a1", seq, &AgentMsg::Harness { session: "s1".into(), json: json.clone() }).await?;
                noted += 1;
                let nack = hub.sent.lock().iter().rev().find_map(|(_, m)| match m {
                    ServerMsg::Wire(Frame::Ack(has)) if has.stream == "s:s1#a1" => has.nacks.iter().find(|n| n.seq == seq).map(|n| n.code),
                    _ => None,
                });
                let want = if code == Code::Unknown { Refusal::Unsupported } else { Refusal::Malformed };
                anyhow::ensure!(nack == Some(want), "{json}: NACKed {nack:?}");
            }
            let kept = hub.refusals.load().kept(&Page::Runner("fedora".into())).cloned().unwrap_or_default();
            anyhow::ensure!(kept.total() == noted, "kept for the runner's page: {} of {noted}", kept.total());
            let harness = runner_cases().into_iter().find(|c| c.of == "Harness").map(|c| c.bad().len()).unwrap_or_default();
            let session = hub.refusals.load().kept(&Page::Session("s1".into())).map(crate::wire::unread::Kept::total);
            anyhow::ensure!(session == Some(2 * u64::try_from(harness)?), "kept for the session's page too: {session:?}");
            // One row a minute for each code (the rest coalesced into the next).
            let rows = refused_rows(&hub, "runner:fedora").await?;
            let firsts: Vec<(String, String)> = rows.iter().map(|(t, r, _)| (t.clone(), r.clone())).collect();
            anyhow::ensure!(firsts == vec![("runner_payload".to_owned(), "malformed".to_owned()), ("runner_payload".to_owned(), "unknown".to_owned())], "{rows:?}");
            Ok(())
        })
    }

    /// A live page's event that's JSON but not one of `StreamSaid`'s kinds goes to no viewer, and
    /// is refused to its runner as a payload that can't be read is: `ServerMsg::Unread` to one that
    /// takes it, noted for its page.
    #[test]
    fn a_live_pages_event_of_a_kind_it_doesnt_know_is_refused_to_its_runner() -> anyhow::Result<()> {
        use crate::hub::tests::{hear, hub_with_runner};
        use crate::proto::{CAP_UNREAD, CAP_WIRE, LANE_CONTROL};
        crate::rt::block_on(async {
            let hub = hub_with_runner(Arc::new(SimClock::new()), "fedora", CAP_UNREAD | CAP_WIRE).await?;
            hear(&hub, 1, LANE_CONTROL, &AgentMsg::StreamEvent { stream: "st1".into(), json: r#"{"hologram":{}}"#.into() }).await?;
            let told = hub.sent.lock().iter().any(|(p, m)| matches!(m, ServerMsg::Unread { wire: Wire::RunnerPayload, what, code: Code::Unknown, .. } if *p == 1 && what == "StreamEvent hologram"));
            anyhow::ensure!(told, "the runner is told: {:?}", hub.sent.lock());
            let kept = hub.refusals.load().kept(&Page::Runner("fedora".into())).map(crate::wire::unread::Kept::total);
            anyhow::ensure!(kept == Some(1), "kept for the runner's page: {kept:?}");
            Ok(())
        })
    }

    /// A runner's frame read off the shard teaches what reading one is expected to take, as one read on
    /// it does: one slow read can't keep a runner's long frames off the shard for good (goal 9, plan
    /// 01a1a87b2cec step 3).
    #[test]
    fn g9_s3_frames_read_off_the_shard_teach_the_expectation() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let data = crate::proto::encode(&AgentMsg::TermOutput { session: "s1".into(), data: vec![7; 4096] })?;
            let len = u64::try_from(data.len())?;
            // As if one read of a frame this long had taken a second: each like it is expected slow, so
            // read off the shard.
            let expect = Expect::default();
            expect.learn(len, 1_000_000_000);
            anyhow::ensure!(expect.expected(len) > hyper_rt::slow::SLOW_POLL_NS, "expected slow");
            for _ in 0..64 {
                let frame = read_frame_by(&expect, data.clone(), true).await;
                anyhow::ensure!(matches!(frame, Frame::Read { carried: Ok(_), .. }), "the frame is read");
            }
            // Each read off the shard took its eighth of the averages: (7/8)^64 of the second is 0.19 ms,
            // plus what the reads took; a hundred milliseconds leaves a debug build's reads plenty of room.
            let left = expect.expected(len);
            anyhow::ensure!(left < 100_000_000, "still expected to take {left} ns");
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 3: a channel item is read as a frame is, the message it carries labeled by
    /// its kind; a channel frame inside one comes back as itself, for the hub to refuse; bytes that aren't a
    /// message are unreadable.
    #[test]
    fn g9_s3_a_channel_item_is_read_as_a_frame_is() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let term = crate::proto::encode(&AgentMsg::TermOutput { session: "s1".into(), data: vec![1, 2, 3] })?;
            match read_item(term).await {
                Item::Read { label, carried: Ok(Carried::Nothing(AgentMsg::TermOutput { .. })) } => anyhow::ensure!(label.name.contains("TermOutput"), "labeled {}", label.name),
                _ => anyhow::bail!("not read as the message it carries"),
            }
            let nested = crate::proto::encode(&AgentMsg::Wire(crate::wire::channel::Frame::Want { stream: "s".into(), from: 1 }))?;
            anyhow::ensure!(matches!(read_item(nested).await, Item::Read { carried: Ok(Carried::Nothing(AgentMsg::Wire(_))), .. }), "a channel frame inside one comes back as itself");
            anyhow::ensure!(matches!(read_item(vec![0xff; 4]).await, Item::Unreadable(_)), "bytes that aren't a message");
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 3: what reading a runner's message is expected to take is kept for each kind
    /// apart, by its variant's index on the wire; a kind past the room, and bytes that don't say one, share the
    /// last; and the room holds every kind there is.
    #[test]
    fn g9_s3_each_kind_of_message_has_its_own_expectation() -> anyhow::Result<()> {
        let term = |session: &str| crate::proto::encode(&AgentMsg::TermOutput { session: session.into(), data: vec![0; 64] });
        let harness = crate::proto::encode(&AgentMsg::Harness { session: "s".into(), json: "{}".into() })?;
        anyhow::ensure!(!std::ptr::eq(costs_of(&term("s")?), costs_of(&harness)), "each kind its own");
        anyhow::ensure!(std::ptr::eq(costs_of(&term("s")?), costs_of(&term("t")?)), "a kind's own, each time");
        anyhow::ensure!(kind_of(&[0x05]) == 5 && kind_of(&[0x81, 0x01]) == (KINDS - 1).min(129), "postcard's varint");
        anyhow::ensure!(kind_of(&[0xff, 0xff, 0xff, 0xff, 0x0f]) == KINDS - 1 && kind_of(&[]) == KINDS - 1, "past the room, or none: the last");
        anyhow::ensure!(crate::wire::agent_classes().len() <= KINDS, "room for every kind: {}", crate::wire::agent_classes().len());
        Ok(())
    }

    /// Goal 9, plan 01a1a87b2cec step 5: on a shard a payload is read straight into its type, and comes out
    /// as it does off one: the same value, a field given twice read as before (its last), the same refusal
    /// of what can't be read. Its unknown fields are still counted, by the checker's thread.
    #[test]
    fn g9_s5_a_payload_read_on_a_shard_is_read_as_off_one() -> anyhow::Result<()> {
        #[derive(Debug, PartialEq, Serialize, serde::Deserialize)]
        struct Two {
            a: u32,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            b: Option<String>,
        }
        const CASES: [&str; 6] = [r#"{"a":1,"b":"x"}"#, r#"{"a":1,"a":2}"#, r#"{"a":"one"}"#, r#"{"a":1"#, "", r#"{"a":1,"new":[1],"empty":null}"#];
        let wire = Wire::StreamDown;
        let off: Vec<Result<Two, Box<Unreadable>>> = CASES.iter().map(|j| payload::<Two>(wire, "Two", j)).collect();
        let before = crate::metrics::wire().ignored.get(&[wire.name()]);
        crate::rt::block_on(async move {
            let on: Vec<Result<Two, Box<Unreadable>>> = CASES.iter().map(|j| payload::<Two>(wire, "Two", j)).collect();
            anyhow::ensure!(on == off, "on a shard {on:?}, off one {off:?}");
            // `new` came with something in it (`empty` didn't): counted once its thread has looked.
            let deadline = Instant::now() + Duration::from_secs(30);
            while crate::metrics::wire().ignored.get(&[wire.name()]).saturating_sub(before) < 1 {
                anyhow::ensure!(Instant::now() < deadline, "the unknown field read on a shard wasn't counted");
                crate::rt::sleep(Duration::from_millis(5)).await;
            }
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 3: each kind of agent session update has its own expectation, found by the tag its
    /// writer puts first; the kinds are every one `Update` has, in its order, as its tag names it. An update that says its
    /// tag elsewhere, or a kind this build doesn't know, shares the expectation of runners' updates as a whole; every other
    /// message keeps its kind's.
    #[test]
    fn g9_s3_each_kind_of_session_update_has_its_own_expectation() -> anyhow::Result<()> {
        use crate::harness::core::Update;
        // `Update`'s variants, as its table has them (every one, matched without a catch-all), in snake case.
        let tags: Vec<String> = crate::wire::update_classes()
            .iter()
            .map(|(name, _)| name.chars().enumerate().fold(String::new(), |mut s, (i, c)| {
                if c.is_ascii_uppercase() && i > 0 {
                    s.push('_');
                }
                s.push(c.to_ascii_lowercase());
                s
            }))
            .collect();
        anyhow::ensure!(tags == UPDATE_TAGS, "the kinds of update are {tags:?}");
        let harness = |u: &Update| -> anyhow::Result<Vec<u8>> { Ok(crate::proto::encode(&AgentMsg::Harness { session: "s1".into(), json: serde_json::to_string(u)? })?) };
        let events = Update::Events { events: Vec::new() };
        let patch = Update::ChangesPatch { set: Vec::new(), gone: Vec::new() };
        for (u, tag) in [(&Update::Closed, "closed"), (&Update::Reset { from: Some(3) }, "reset"), (&events, "events"), (&patch, "changes_patch")] {
            let json = serde_json::to_string(u)?;
            anyhow::ensure!(update_kind(json.as_bytes()) == UPDATE_TAGS.iter().position(|t| *t == tag), "{json}: its kind");
            let data = harness(u)?;
            anyhow::ensure!(UPDATE_TAGS.iter().position(|t| *t == tag).and_then(|k| UPDATE_COSTS.get(k)).is_some_and(|e| std::ptr::eq(costs_of(&data), e)), "{json}: its own expectation");
        }
        anyhow::ensure!(!std::ptr::eq(costs_of(&harness(&events)?), costs_of(&harness(&patch)?)), "each kind its own");
        // Its tag elsewhere, or a kind this build doesn't know: runners' updates' expectation, as before.
        let whole = KIND_COSTS.get(kind_of(&harness(&events)?)).unwrap_or(&REST_COSTS);
        for said in [r#"{"events":[],"t":"events"}"#, r#"{"t":"teleport"}"#, r#"{ "t":"events","events":[]}"#, "", "{"] {
            let data = crate::proto::encode(&AgentMsg::Harness { session: "s1".into(), json: said.into() })?;
            anyhow::ensure!(std::ptr::eq(costs_of(&data), whole), "{said}: runners' updates' expectation");
        }
        let term = crate::proto::encode(&AgentMsg::TermOutput { session: "s1".into(), data: vec![1; 8] })?;
        anyhow::ensure!(std::ptr::eq(costs_of(&term), KIND_COSTS.get(kind_of(&term)).unwrap_or(&REST_COSTS)), "another message its kind's");
        Ok(())
    }

    /// Goal 9, plan 01a1a87b2cec step 3: what a frame is expected to hold the shard for is its read where that's expected on
    /// the shard, and a channel item's frame adds the read of the message it carries; a read expected past a slow poll is
    /// done off the shard, so it adds nothing; nothing's expected of what nothing has been learned of.
    #[test]
    fn g9_s3_a_frames_expected_hold_counts_the_message_its_item_carries() -> anyhow::Result<()> {
        use crate::wire::channel::Frame;
        let body = crate::proto::encode(&AgentMsg::Harness { session: "s1".into(), json: r#"{"t":"closed"}"#.into() })?;
        let item = crate::proto::encode(&AgentMsg::Wire(Frame::Item { stream: "s:s1#a1".into(), seq: 2, low: 1, body: body.clone() }))?;
        let len = |b: &[u8]| u64::try_from(b.len()).unwrap_or(u64::MAX);
        // A frame read at 10 ns a byte, its message at 20.
        let (frames, messages) = (Expect::default(), Expect::default());
        let costs = |data: &[u8]| if crate::proto::harness_at(data).is_some() { &messages } else { &frames };
        anyhow::ensure!(expected_by(&item, costs) == 0, "nothing learned, nothing expected");
        frames.learn(1_000, 10_000);
        messages.learn(1_000, 20_000);
        anyhow::ensure!(expected_by(&body, costs) == 20 * len(&body), "a message alone: its own read");
        anyhow::ensure!(expected_by(&item, costs) == 10 * len(&item) + 20 * len(&body), "an item: its frame's read and its message's");
        // Its message's read expected past a slow poll: done off the shard, so the item's frame alone holds it.
        let slow = Expect::default();
        slow.learn(1, hyper_rt::slow::SLOW_POLL_NS);
        let off = |data: &[u8]| if crate::proto::harness_at(data).is_some() { &slow } else { &frames };
        anyhow::ensure!(expected_by(&item, off) == 10 * len(&item), "a message read off the shard adds nothing");
        Ok(())
    }

    /// Goal 9, plan 01a1a87b2cec step 5: a payload the reader hands over reads as one it lends, on a shard and off one: the
    /// same value, the same refusal of what can't be read; and its unknown fields are still counted, by the checker's
    /// thread, the text handed to it as it came.
    #[test]
    fn g9_s5_a_payload_handed_over_reads_as_one_lent() -> anyhow::Result<()> {
        #[derive(Debug, PartialEq, Serialize, serde::Deserialize)]
        struct Two {
            a: u32,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            b: Option<String>,
        }
        const CASES: [&str; 6] = [r#"{"a":1,"b":"x"}"#, r#"{"a":1,"a":2}"#, r#"{"a":"one"}"#, r#"{"a":1"#, "", r#"{"a":1,"handed":[1],"empty":null}"#];
        // A wire no other test reads JSON on, so the count is this test's alone.
        let wire = Wire::LogDown;
        let lent: Vec<Result<Two, Box<Unreadable>>> = CASES.iter().map(|j| payload::<Two>(wire, "Two", j)).collect();
        let handed: Vec<Result<Two, Box<Unreadable>>> = CASES.iter().map(|j| payload_owned::<Two>(wire, "Two", (*j).to_owned())).collect();
        anyhow::ensure!(handed == lent, "off a shard: handed over {handed:?}, lent {lent:?}");
        let before = crate::metrics::wire().ignored.get(&[wire.name()]);
        crate::rt::block_on(async move {
            let on: Vec<Result<Two, Box<Unreadable>>> = CASES.iter().map(|j| payload_owned::<Two>(wire, "Two", (*j).to_owned())).collect();
            anyhow::ensure!(on == lent, "on a shard {on:?}, off one {lent:?}");
            // `handed` came with something in it (`empty` didn't): counted once its thread has looked.
            let deadline = Instant::now() + Duration::from_secs(30);
            while crate::metrics::wire().ignored.get(&[wire.name()]).saturating_sub(before) < 1 {
                anyhow::ensure!(Instant::now() < deadline, "the unknown field of a payload handed over wasn't counted");
                crate::rt::sleep(Duration::from_millis(5)).await;
            }
            Ok(())
        })
    }

    /// No handler reads JSON of its own: in the hub, the runner and the harness's host, a read of
    /// what came on a wire is [`payload`]'s, and every other read is of something of the machine's
    /// own (a file, a socket of its own, what it's about to send), each named here.
    #[test]
    fn no_handler_reads_a_payload_of_its_own() {
        const OWN: [(&str, &str); 7] = [
            ("serde_json::from_slice(&json).ok()?", "a session handed back by this machine's newer build, on a socket of its own"),
            ("serde_json::from_value(cut_value(v, budget))", "its own event, cut to fit a frame before it's sent"),
            ("let handed: Vec<HandedTerm> = match serde_json::from_slice(&raw)", "terminals the runner before it handed over, in a file"),
            ("Ok(raw) => serde_json::from_slice(&raw).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),", "its records of hand-overs, in files of its own"),
            ("serde_json::from_slice(&raw).unwrap_or_else(|e| {", "the sessions it handed itself, and its record of its sessions, in its files"),
            ("let state: super::model::State = kept.state.as_deref().and_then(|s| serde_json::from_str(s).ok())?;", "what its conversation store kept"),
            ("let mut events: Vec<super::model::Event> = kept.events.iter().filter_map(|e| serde_json::from_str(&e.body).ok()).collect();", "what its conversation store kept"),
        ];
        const READS: [&str; 7] = ["serde_json::from_str", "serde_json::from_slice", "serde_json::from_value", "serde_json::from_reader", "feed::decode(", "learner::decode(", "wire::decode("];
        let sources = [("hub.rs", include_str!("../hub.rs")), ("agent.rs", include_str!("../agent.rs")), ("harness/host.rs", include_str!("../harness/host.rs"))];
        let mut found = Vec::new();
        for (file, text) in sources {
            // Up to its tests.
            let code = text.split("\n#[cfg(test)]\nmod tests").next().unwrap_or(text).split("\n#[cfg(test)]\npub(crate) mod tests").next().unwrap_or(text);
            for (n, line) in code.lines().enumerate() {
                if READS.iter().any(|r| line.contains(r)) && !OWN.iter().any(|(own, _)| line.contains(own)) {
                    found.push(format!("{file}:{}: {}", n + 1, line.trim()));
                }
            }
        }
        assert!(found.is_empty(), "reads of a payload outside wire::payload: {} calls\n{}", found.len(), found.join("\n"));
    }
}
