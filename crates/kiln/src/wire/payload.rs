//! What frames carry (docs/wire.md, "What can't be read"): the JSON in a runner's and the server's
//! messages, read once, where its frame is read, into the type it is; a runner's custody record where
//! it's taken, off the hub's loop ([`Later`]).
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
//! such field of each payload looked at, and logged once an hour for each wire and field. It's never
//! refused or audited. One payload of a kind (a wire and a message's variant) is looked at at a time,
//! the next once the last is done ([`looks_at`]): off a shard where it's read, on one by the compute
//! pool's workers with time to spare ([`check_spare`]); those looked at are counted in
//! `kiln_wire_checked_payloads_total{wire}`, and those waiting in `kiln_wire_checks_waiting`. serde can't say which fields it
//! ignored inside an internally tagged enum (it reads one whole first), and most of these payloads are
//! one, so they're found by writing the value read back and seeing what of what came it lacks.

use std::collections::BTreeSet;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use kiln_http::turn::Costs;
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
/// have are taken, counted and said in the log ([`Ignored`]), of a payload looked at ([`looks_at`]).
///
/// On a shard it's read straight into its type, with no JSON tree between, and its unknown fields are
/// looked for off the shard, by the compute pool's spare time ([`check_spare`]; goal 9, plan 01a1a87b2cec step 5: looking read each
/// runner message's JSON into a tree, the type out of that, and the type back into another tree to
/// compare, on the hub's shard). Off one, one looked at is read through a tree, its unknown fields found
/// as it's read, and the rest straight. What can't be read straight is read the way it always was, so
/// it's refused, and said why, as before.
pub fn payload<T: DeserializeOwned + Serialize>(wire: Wire, of: &'static str, json: &str) -> Result<T, Box<Unreadable>> {
    let look = looks_at(wire, of);
    if (look.is_none() || crate::rt::on_shard())
        && let Ok(read) = serde_json::from_str::<T>(json)
    {
        if let Some(looking) = look {
            check_later(wire, of, json, unknown_in::<T>, looking);
        }
        return Ok(read);
    }
    // Looked at here, if at all: its kind's next is looked at once this read is done (`look` let go).
    read_by_tree(wire, of, json, look.is_some())
}

/// [`payload`] of JSON the reader holds and has no more use for: on a shard, read straight into its type, then handed
/// as it is to the compute pool's spare time to look for its unknown fields in ([`check_spare`]), where `payload` copies it for that (goal 9, plan
/// 01a1a87b2cec step 5: each runner payload's text was copied whole on the hub's loop for its check). The same value,
/// and the same refusal of what can't be read.
pub fn payload_owned<T: DeserializeOwned + Serialize>(wire: Wire, of: &'static str, json: String) -> Result<T, Box<Unreadable>> {
    let look = looks_at(wire, of);
    if (look.is_none() || crate::rt::on_shard())
        && let Ok(read) = serde_json::from_str::<T>(&json)
    {
        if let Some(looking) = look {
            check_owned(wire, of, json, unknown_in::<T>, looking);
        }
        return Ok(read);
    }
    // Looked at here, if at all: its kind's next is looked at once this read is done (`look` let go).
    read_by_tree(wire, of, &json, look.is_some())
}

/// [`payload_owned`] of JSON lying in bytes the reader has no more use for (`data` at `json`: the frame it came in): on a shard,
/// read straight from where it lies, its strings' UTF-8 checked as they're read, then the bytes handed as they are to the
/// compute pool's spare time to look for its unknown fields in ([`check_spare`]). The same value, and the same refusal of
/// what can't be read; text that isn't UTF-8 is refused as `malformed`, saying so (goal 9, plan 01a1a87b2cec step 3).
pub fn payload_framed<T: DeserializeOwned + Serialize>(wire: Wire, of: &'static str, data: Vec<u8>, json: std::ops::Range<usize>) -> Result<T, Box<Unreadable>> {
    let bytes = data.get(json.clone()).unwrap_or_default();
    let look = looks_at(wire, of);
    if (look.is_none() || crate::rt::on_shard())
        && let Ok(read) = serde_json::from_slice::<T>(bytes)
    {
        if let Some(looking) = look {
            check_framed(wire, of, data, json, unknown_in::<T>, looking);
        }
        return Ok(read);
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => read_by_tree(wire, of, text, look.is_some()),
        Err(e) => Err(Box::new(Unreadable { of, what: of.to_owned(), code: Code::Malformed, why: format!("not UTF-8: {e}"), session: None, waits: None })),
    }
}

/// [`payload`]'s read off a shard of one looked at, and of what couldn't be read straight: through a JSON tree, so what can't
/// be read is refused saying why, and what of it the type doesn't keep is found as it's read, where it's looked at (`look`).
fn read_by_tree<T: DeserializeOwned + Serialize>(wire: Wire, of: &'static str, json: &str, look: bool) -> Result<T, Box<Unreadable>> {
    let came = tree(of, json)?;
    let read = T::deserialize(&came).map_err(|e| refused(of, &e))?;
    if look && let Ok(kept) = serde_json::to_value(&read) {
        found(wire, of, &unknown_fields(&came, &kept));
    }
    Ok(read)
}

/// `json`, what a message of variant `of` carries, read as a JSON tree: what isn't JSON is refused as `malformed`, saying
/// why ([`read_by_tree`]'s first step, and all of [`Later::read`]).
fn tree(of: &'static str, json: &str) -> Result<Value, Box<Unreadable>> {
    serde_json::from_str(json).map_err(|e| not_json(of, &e))
}

/// `json`, what a message of variant `of` carries, read as JSON and kept as the text it came as, with no tree made of it
/// (`FromRunner::HeadroomStatus`; goal 9, plan 01a1a87b2cec step 5): what isn't JSON is refused as `malformed`, saying why, as
/// [`tree`] refuses it.
fn json_text(of: &'static str, json: String) -> Result<String, Box<Unreadable>> {
    match serde_json::from_str::<serde::de::IgnoredAny>(&json) {
        Ok(_) => Ok(json),
        Err(e) => Err(not_json(of, &e)),
    }
}

/// What a message of variant `of` carried that isn't JSON, refused as `malformed`, saying why.
fn not_json(of: &'static str, e: &serde_json::Error) -> Box<Unreadable> {
    Box::new(Unreadable {
        of,
        what: of.to_owned(),
        code: Code::Malformed,
        why: if e.is_eof() { "cut short".to_owned() } else { format!("not JSON: {e}") },
        session: None,
        waits: None,
    })
}

/// What a message carries, read where it's taken rather than where its frame is read ([`payload_of`]): its JSON as it
/// came, held as text (goal 9, plan 01a1a87b2cec step 3). A runner's custody record (`Custody`) is read so, by custody's
/// intake, off the hub's loop and in the order its runner sent it (`crate::custody::runners::Intake`): the hub's loop
/// read each into a JSON tree on its shard (train 16's lab, g9p16: polls labeled `runner AgentMsg::Custody` held their
/// shard 0.30 to 0.48 ms on the CPU).
pub struct Later {
    of: &'static str,
    json: String,
}

impl Later {
    /// Its JSON, read as [`payload`] reads a JSON value: a value keeps every field it came with, so there's nothing to look
    /// for in it past reading it; what isn't JSON is refused as `malformed`, saying why, as it was where its frame was read.
    pub fn read(self) -> Result<Value, Box<Unreadable>> {
        tree(self.of, &self.json)
    }

    /// The bytes it holds: its text's buffer.
    pub fn bytes(&self) -> usize {
        self.json.capacity()
    }
}

/// A payload of variant `of` on `wire` looked at, `fields` what it carried that this build doesn't know: counted as looked at
/// (`kiln_wire_checked_payloads_total`), and its fields counted and said ([`say_unknown`]).
fn found(wire: Wire, of: &'static str, fields: &[String]) {
    crate::metrics::wire().checked.add(&[wire.name()], 1);
    say_unknown(wire, of, fields);
}

/// The most kinds of payload [`looks_at`] keeps apart, each a wire and a message's variant: the variants of [`AgentMsg`] and
/// [`ServerMsg`] fit [`KINDS`] each, and every other wire carries a few kinds, so twice their room keeps it under half full,
/// where a linear probe finds a kind it holds in 1.5 looks on average (Knuth, TAOCP vol. 3, 6.4: (1 + 1/(1 - a)) / 2 at a
/// load a of 1/2). A kind past them is looked at every time, as every payload was.
const KINDS_LOOKED: usize = 4 * KINDS;

/// A kind's place in [`LOOKED`]: its key (a hash of its wire and variant, never 0; 0: free) and whether a payload of it is
/// being looked at now ([`Looking`]).
struct Looked {
    kind: std::sync::atomic::AtomicU64,
    busy: std::sync::atomic::AtomicBool,
}

/// Whether each kind of payload is being looked at now ([`looks_at`]), claimed a kind a place as it first comes, with no lock.
static LOOKED: [Looked; KINDS_LOOKED] = [const { Looked { kind: std::sync::atomic::AtomicU64::new(0), busy: std::sync::atomic::AtomicBool::new(false) } }; KINDS_LOOKED];

/// A look at a payload under way ([`looks_at`]): its kind's place in [`LOOKED`], let go as it ends (dropped: done, or the
/// payload let go), so the kind's next payload is looked at. None for a kind past the table, looked at every time.
pub(crate) struct Looking(Option<&'static Looked>);

impl Drop for Looking {
    fn drop(&mut self) {
        if let Some(place) = self.0 {
            place.busy.store(false, std::sync::atomic::Ordering::Release);
        }
    }
}

/// The place of the kind of payload of variant `of` on `wire` in [`LOOKED`], claimed for it as it first comes: none where
/// the table holds no room for it.
fn place_of(wire: Wire, of: &str) -> Option<&'static Looked> {
    use std::sync::atomic::Ordering::Relaxed;
    let key = kind_key(wire, of);
    let start = usize::try_from(key % KINDS_LOOKED as u64).unwrap_or(0);
    // Its place read first, and claimed only where free: a compare-and-swap that finds its kind there takes the line as a
    // write would, from every shard that reads the kind.
    (0..KINDS_LOOKED).filter_map(|i| LOOKED.get((start + i) % KINDS_LOOKED)).find(|p| match p.kind.load(Relaxed) {
        0 => match p.kind.compare_exchange(0, key, Relaxed, Relaxed) {
            Ok(_) => true,
            Err(held) => held == key,
        },
        held => held == key,
    })
}

/// A kind's key in [`LOOKED`]: FNV-1a's 64 bits of its wire's name and its variant, apart, never 0.
fn kind_key(wire: Wire, of: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in wire.name().bytes().chain(std::iter::once(0)).chain(of.bytes()) {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    h.max(1)
}

/// Whether a payload of variant `of` on `wire` is looked at for fields this build doesn't know, and its look ([`Looking`]):
/// one of a kind at a time, the next once the last is done (consts-audit R3; goal 9, plan 01a1a87b2cec step 5). A checker
/// with time to spare looks at a kind's every payload; one with none at the first of them, the rest read straight while it
/// waits. It was one of a kind every 100 ms (`CHECK_EVERY`, the shards' counts' age on /metrics), whatever the checker had
/// time for: every payload had been, each handed to a thread of its own from the shard that read it, 0.14% of the lab's
/// samples on the shards, and read into trees twice more there, 2.9%, a quarter of the shards' own. Taken with a
/// compare-and-swap, so of payloads of a kind at once only one is looked at. A kind past the table's room is looked at every
/// time, as every payload was.
fn looks_at(wire: Wire, of: &str) -> Option<Looking> {
    use std::sync::atomic::Ordering::{Acquire, Relaxed};
    let Some(place) = place_of(wire, of) else { return Some(Looking(None)) };
    // Read first: a kind under way is the common case, and a compare-and-swap would take the line from the checker.
    if place.busy.load(Relaxed) {
        return None;
    }
    place.busy.compare_exchange(false, true, Acquire, Relaxed).is_ok().then_some(Looking(Some(place)))
}

/// Whether a look at a payload of variant `of` on `wire` is under way (a test's).
#[cfg(test)]
fn looking(wire: Wire, of: &str) -> bool {
    place_of(wire, of).is_some_and(|p| p.busy.load(std::sync::atomic::Ordering::Acquire))
}

/// The look at payloads of variant `of` on `wire` ([`looks_at`]), taken once whatever look at one of them is under way is done
/// (the compute pool given turns meanwhile: its spare time is the checker's), and held till it's dropped: a test's, whose
/// reads of the kind meanwhile are read straight, none handed on to be looked at. 30 s at the most.
#[cfg(test)]
pub(crate) async fn look_held(wire: Wire, of: &str) -> anyhow::Result<Looking> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(look) = looks_at(wire, of) {
            return Ok(look);
        }
        anyhow::ensure!(Instant::now() < deadline, "the look at a {of} on {} under way wasn't done in 30 s", wire.name());
        crate::rt::compute(|| ()).await?;
        crate::rt::sleep(Duration::from_millis(1)).await;
    }
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

/// A payload read on a shard, for the compute pool's spare time to look for its unknown fields in (`find`), and its look
/// under way, which its kind's next waits for ([`Looking`]).
struct Check {
    wire: Wire,
    of: &'static str,
    /// The bytes its JSON lies in, at `json`: its text alone, or the frame it came in.
    text: Vec<u8>,
    json: std::ops::Range<usize>,
    find: fn(&str) -> Vec<String>,
    _looking: Looking,
}

impl Check {
    /// Its unknown fields, as `find` finds them in its JSON.
    fn unknown(&self) -> Vec<String> {
        self.text.get(self.json.clone()).and_then(|b| std::str::from_utf8(b).ok()).map(self.find).unwrap_or_default()
    }
}

/// The most bytes of payloads waiting to be looked at ([`check_spare`]): as many as the hub's loop keeps waiting from
/// runners (`crate::net::EVENTS_BYTES`), so they fall no further behind than the hub may. One of a kind at a time waits
/// ([`looks_at`]), so it's past this only with that many bytes of kinds. Past it, one is let go unlooked at, counted, and
/// said at each power of two.
const CHECKS_MOST: usize = crate::net::EVENTS_BYTES;

impl crate::memory::queue::Weigh for Check {
    /// Its place and the bytes its payload lies in, with the room their buffer has past them ([`payload_framed`] hands on
    /// the frame it came in).
    fn weight(&self) -> usize {
        size_of::<Check>().saturating_add(self.text.capacity())
    }
}

/// The payloads let go unlooked at for want of room ([`CHECKS_MOST`]).
static CHECKS_LET_GO: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Hands a payload read on a shard to the compute pool's spare time to look for its unknown fields in ([`check_spare`]), a
/// copy of it; where there's no such pool's spare time to hand it to, they're looked for here, as before. `looking`: its
/// look, let go once it's done.
fn check_later(wire: Wire, of: &'static str, json: &str, find: fn(&str) -> Vec<String>, looking: Looking) {
    if checks().is_none() {
        found(wire, of, &find(json));
        return;
    }
    check_owned(wire, of, json.to_owned(), find, looking);
}

/// [`check_later`] of a payload the reader has no more use for: handed on as it is.
fn check_owned(wire: Wire, of: &'static str, json: String, find: fn(&str) -> Vec<String>, looking: Looking) {
    let len = json.len();
    check_framed(wire, of, json.into_bytes(), 0..len, find, looking);
}

/// [`check_owned`] of a payload lying in bytes the reader has no more use for (`text` at `json`: the frame it came in).
fn check_framed(wire: Wire, of: &'static str, text: Vec<u8>, json: std::ops::Range<usize>, find: fn(&str) -> Vec<String>, looking: Looking) {
    use std::sync::atomic::Ordering::Relaxed;
    let check = Check { wire, of, text, json, find, _looking: looking };
    let Some((tx, _)) = checks() else {
        found(wire, of, &check.unknown());
        return;
    };
    // Counted before it's handed over, so a worker that takes it at once never counts below none.
    CHECKS_WAITING.fetch_add(1, Relaxed);
    match tx.try_send(check) {
        Ok(()) => {}
        Err(async_channel::TrySendError::Full(_)) => {
            CHECKS_WAITING.fetch_sub(1, Relaxed);
            let n = CHECKS_LET_GO.fetch_add(1, Relaxed).saturating_add(1);
            if n.is_power_of_two() {
                tracing::warn!(let_go = n, "runners' messages weren't looked at for fields this build doesn't know: the compute pool has had no time to spare");
            }
        }
        // None to take it any more: looked for here.
        Err(async_channel::TrySendError::Closed(c)) => {
            CHECKS_WAITING.fetch_sub(1, Relaxed);
            found(c.wire, c.of, &c.unknown());
        }
    }
}

/// Payloads waiting to be looked at ([`check_spare`]): what `kiln_wire_checks_waiting` says. A compute pool with no time to
/// spare (or none: no job ends, so no worker finds itself free) leaves them waiting (consts-audit R3).
static CHECKS_WAITING: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many payloads wait to be looked at, for `/metrics`.
pub fn checks_waiting() -> u64 {
    CHECKS_WAITING.load(std::sync::atomic::Ordering::Relaxed)
}

/// Payloads read on the shards waiting to be looked at, for the compute pool's workers to take in their spare time
/// ([`check_spare`], `hyper_rt::idle::when_spare`; consts-audit R3), made the first time one is: a worker that finds no job
/// takes one before it rests, so none waits for a wake, and none is looked at but with time a worker had to spare. It was
/// a thread of its own, looking every 100 ms (`CHECK_EVERY`, the shards' counts' age on /metrics) and sleeping between
/// (goal 9, plan 01a1a87b2cec step 5: woken for each, it had cost a futex's wake on the hub's shard a payload). None where
/// the process gave the pool's spare time to other work (said so): looked for where they're read, as before.
fn checks() -> Option<&'static (crate::memory::queue::Sender<Check>, crate::memory::queue::Receiver<Check>)> {
    static CHECKS: OnceLock<Option<(crate::memory::queue::Sender<Check>, crate::memory::queue::Receiver<Check>)>> = OnceLock::new();
    CHECKS
        .get_or_init(|| {
            if !hyper_rt::idle::when_spare(check_spare) {
                tracing::warn!("the compute pool's spare time is another's: fields this build doesn't know in runners' messages are looked for where they're read");
                return None;
            }
            Some(crate::memory::queue::bounded::<Check>(&crate::memory::owners::WIRE_CHECKS, CHECKS_MOST))
        })
        .as_ref()
}

/// The compute pool's spare work ([`checks`]): a payload waiting looked at, its kind's next looked at once it's done (its
/// [`Looking`] let go). Whether one waited.
fn check_spare() -> bool {
    let Some((_, rx)) = checks() else { return false };
    match rx.try_recv() {
        Ok(c) => {
            CHECKS_WAITING.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            found(c.wire, c.of, &c.unknown());
            true
        }
        Err(_) => false,
    }
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

/// Where `len` units of work whose kind costs `costs` a unit is done, weighed against `sides` (a shard's quantum and the
/// compute pool's round trip): on the shard where it's expected to cost no more than the pool's round trip and the quantum,
/// the rule work is weighed by with nothing free (`kiln_http::turn::Costs::fit`), and then what it's expected to take
/// there; off it (none) past that, and while nothing is learned of its kind or of the pool, the first of each going to the
/// pool, whose run teaches both. Off a shard, where it's asked. What a runner frame's read takes is learned by its bytes
/// ([`read_frame`]), as making the server's frames is (`crate::net::MADE`) and parsing a hub event (`crate::push`). Goal 9,
/// plan 01a1a87b2cec step 3: they were weighed by RFC 6298's smoothing (a new sample's weight an eighth, its variation's a
/// quarter, four variations), gains no measurement picked.
pub(crate) fn on_shard(costs: &Costs, len: u64, sides: crate::rt::Sides) -> Option<u64> {
    let expected = expected(costs, len);
    let Some(quantum) = sides.quantum_ns else { return Some(expected) };
    costs.fit(0, sides.trip_ns.map_or(0, |trip| trip.min(quantum))).filter(|fits| *fits >= len.max(1)).map(|_| expected)
}

/// What `len` units of a kind that costs `costs` a unit are expected to take, nanoseconds: none of what nothing is learned of.
fn expected(costs: &Costs, len: u64) -> u64 {
    costs.mean().map_or(0, |ns| (ns * len as f64) as u64)
}

/// Format: room in [`KIND_COSTS`] for [`AgentMsg`]'s variants by their index on the wire (its table,
/// `crate::wire::agent_classes`, has 70-some; a test holds it within), a kind past it sharing the rest's.
const KINDS: usize = 128;

/// What reading a runner's message of each kind is expected to take, as the hub's loop reads frames
/// ([`read_frame`]) and channel items ([`read_item`]), a byte a unit: one for each of [`AgentMsg`]'s variants (goal
/// 9, plan 01a1a87b2cec step 3). One for all judged an agent session's update, whose JSON costs much more a
/// byte, by the many messages that carry bytes (terminal output, streams): mid-sized updates were read on
/// the shard, up to 2.4 ms each in the lab's g6c2.
static KIND_COSTS: [Costs; KINDS] = [const { Costs::nothing_yet() }; KINDS];

/// What [`KIND_COSTS`] has no room for: kinds past it, and what doesn't say its kind.
static REST_COSTS: Costs = Costs::nothing_yet();

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

/// What reading an agent session's update of each kind is expected to take, a byte a unit: one for each of [`UPDATE_TAGS`]
/// (goal 9, plan 01a1a87b2cec step 3), as [`KIND_COSTS`] keeps one for each kind of message. Their JSON costs a byte
/// far apart: a change's hunks and a session's state are many short fields, an event's reply one long text. Under one
/// expectation for all of them, a long change was read on the shard at the rate the long replies before it had set.
static UPDATE_COSTS: [Costs; UPDATE_TAGS.len()] = [const { Costs::nothing_yet() }; UPDATE_TAGS.len()];

/// The kind of agent session update `json` is: its tag's place in [`UPDATE_TAGS`], where the tag comes first, as every
/// writer of an update puts it (serde's tagged serializer: `crate::harness::core::Update`); none for an update that says
/// it otherwise, or a kind this build doesn't know.
fn update_kind(json: &[u8]) -> Option<usize> {
    let rest = json.strip_prefix(br#"{"t":""#)?;
    let tag = rest.get(..rest.iter().position(|b| *b == b'"')?)?;
    UPDATE_TAGS.iter().position(|t| t.as_bytes() == tag)
}

/// What runner message `message` is learned by, by its kind: what reading it takes ([`KIND_COSTS`]) and what taking it takes
/// past its read ([`PIECES`]); an agent session's update by its own kind of update ([`UPDATE_COSTS`], [`UPDATE_PIECES`]). Its
/// head read once for both (goal 9, plan 01a1a87b2cec step 3: read four times a channel item, by each that asked).
fn kind_costs(message: &[u8]) -> (&'static Costs, &'static Costs) {
    if let Some(k) = crate::proto::harness_at(message).and_then(|update| update_kind(update.json))
        && let (Some(costs), Some(pieces)) = (UPDATE_COSTS.get(k), UPDATE_PIECES.get(k))
    {
        return (costs, pieces);
    }
    let k = kind_of(message);
    (KIND_COSTS.get(k).unwrap_or(&REST_COSTS), PIECES.get(k).unwrap_or(&REST_PIECES))
}

/// What taking a runner's message of each kind holds the hub's loop's shard for past its read, a piece a unit, as
/// [`KIND_COSTS`] keeps what its reads take a byte: the rest of its read as it went, and what its handler did before it first
/// waited, nanoseconds on the CPU (`hyper_rt::futures::thread_run_ns`), else on the clock ([`Piece::take`]). One for each of
/// [`AgentMsg`]'s variants by its index on the wire, a channel item's by the message it carries (goal 9, plan 01a1a87b2cec
/// step 3).
static PIECES: [Costs; KINDS] = [const { Costs::nothing_yet() }; KINDS];

/// An agent session's update's, by its own kind ([`UPDATE_TAGS`]), as [`UPDATE_COSTS`] keeps their reads': an inbox's
/// answer is taken on the hub's loop, the rest handed to their owners.
static UPDATE_PIECES: [Costs; UPDATE_TAGS.len()] = [const { Costs::nothing_yet() }; UPDATE_TAGS.len()];

/// What [`PIECES`] has no room for: kinds past it, and what doesn't say its kind.
static REST_PIECES: Costs = Costs::nothing_yet();

/// A runner's job output, its log chunks ([`Piece::job_log`]): no runner message, read whole and written where the hub's
/// loop takes it.
static LOG_PIECES: Costs = Costs::nothing_yet();

/// Where a runner message is read, as its piece decided once ([`Piece::of`]): on the shard, expected to take what it says
/// there, nanoseconds, or on the compute pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Read {
    Shard(u64),
    Pool,
}

/// A runner's frame as a piece of the hub's loop's turn on its shard (goal 9, plan 01a1a87b2cec step 3: no poll past the
/// quantum): whether taking it fits what's left of its turn's quantum ([`Piece::fits`]), so the loop gives its shard up
/// first where it won't (`crate::rt::Turns::room_for`); and what it took there, learned as it's taken ([`Piece::take`]).
/// The loop gave its shard up only before a frame whose read, as expected, wouldn't fit: a cheap read with a dearer
/// handler, and a read that took more than it was expected to, held its polls past the quantum all the same (train 16's
/// lab, g9p16: its labels to 0.36 ms on the CPU in the window, 0.48 ms custody's, as their turns' last pieces).
#[derive(Clone, Copy)]
pub struct Piece {
    /// Where the message it holds is read (a channel item's, the one it carries), decided once: the read is done where its
    /// piece was weighed ([`read_frame`], [`read_item`]).
    read: Read,
    /// What its kind's reads are learned by, a byte a unit.
    costs: &'static Costs,
    /// What its kind takes past its read, a piece a unit.
    rest: &'static Costs,
}

impl Piece {
    /// Runner frame `data`: the message it holds, a channel item's the one it carries, whose head the loop reads where it
    /// lies (a few varints and its stream's name: nothing to place). Where its message is read is decided here, once, for
    /// the whole piece, its read and what its kind takes past it fitting a turn together where it's on the shard (goal 9,
    /// plan 01a1a87b2cec step 3: decided again as the read was done, against what the loops had learned since, a read
    /// counted off the shard could run on it, past the quantum; and the shard read an item's head as a frame of its own).
    pub fn of(data: &[u8]) -> Self {
        let message = crate::proto::item_at(data).map_or(data, |item| item.body);
        let (costs, rest) = kind_costs(message);
        Self::placed(message, costs, rest, crate::rt::Sides::here())
    }

    /// Message `message`'s piece, learned by `costs` and `rest`, its read placed against `sides`: on the shard where it's no
    /// dearer there ([`on_shard`]) and the whole piece fits a turn, its read as expected and what its kind takes past it at
    /// the dearest it has been ([`Piece::fits`]); with nothing learned past its read, by its read alone.
    fn placed(message: &[u8], costs: &'static Costs, rest: &'static Costs, sides: crate::rt::Sides) -> Self {
        let len = u64::try_from(message.len()).unwrap_or(u64::MAX);
        let fits = |read: &u64| sides.quantum_ns.is_none_or(|q| rest.fit(kiln_http::turn::signed(q).saturating_sub(kiln_http::turn::signed(*read)), 0).is_none_or(|n| n >= 1));
        let read = on_shard(costs, len, sides).filter(fits).map_or(Read::Pool, Read::Shard);
        Self { read, costs, rest }
    }

    /// Frame `data`'s piece, its message read on the shard whatever its kind has learned (a test's).
    #[cfg(test)]
    pub(crate) fn on_shard(data: &[u8]) -> Self {
        let message = crate::proto::item_at(data).map_or(data, |item| item.body);
        let (costs, rest) = kind_costs(message);
        Self { read: Read::Shard(0), costs, rest }
    }

    /// A runner's frame of job output (a log chunk): read whole and written where the hub's loop takes it, all of it the
    /// piece's own.
    pub fn job_log() -> Self {
        Self { read: Read::Shard(0), costs: &REST_COSTS, rest: &LOG_PIECES }
    }

    /// Whether it fits `room` nanoseconds of its turn (negative: the turn is past its quantum by that): its read on the shard
    /// as expected, and what its kind takes past its read at the dearest it has been, the rule work is weighed by with nothing
    /// past the room put off (`kiln_http::turn::Costs::fit`). Until one of its kind has been taken no room is enough: it's
    /// its turn's first piece. Goal 9, plan 01a1a87b2cec step 3: RFC 6298's bound (the mean and four variations) forgot a
    /// kind's rare dear piece within a dozen, and the next was taken into a turn it didn't fit.
    pub fn fits(&self, room: i64) -> bool {
        let read = match self.read {
            Read::Shard(ns) => ns,
            Read::Pool => 0,
        };
        self.rest.fit(room.saturating_sub(kiln_http::turn::signed(read)), 0).is_some_and(|n| n >= 1)
    }

    /// `work`, the piece's, done, and what its first poll took past its read's expectation learned for its kind (`rest`):
    /// the time it held its shard before it first waited, what it adds to the turn it starts in. What it does after a wait
    /// comes in a later poll, of the same turn while that has room left (its clock runs through the wait). A piece read off
    /// the shard teaches nothing: its first poll ends at its read, before its handler. With no piece, `work` as it is.
    pub async fn take<F: std::future::Future>(piece: Option<Self>, work: F) -> F::Output {
        let mut work = std::pin::pin!(work);
        let mut first = piece.and_then(|p| match p.read {
            Read::Shard(ns) => Some((p.rest, ns)),
            Read::Pool => None,
        });
        std::future::poll_fn(move |cx| {
            let Some((rest, read_ns)) = first.take() else { return work.as_mut().poll(cx) };
            let (cpu, began) = (hyper_rt::futures::thread_run_ns(), Instant::now());
            let polled = work.as_mut().poll(cx);
            let took = match (cpu, hyper_rt::futures::thread_run_ns()) {
                (Some(from), Some(to)) => to.saturating_sub(from),
                _ => u64::try_from(began.elapsed().as_nanos()).unwrap_or(u64::MAX),
            };
            rest.learn(1, took.saturating_sub(read_ns));
            polled
        })
        .await
    }
}

/// Reads runner frame `data` (postcard) and what its message carries (JSON, [`payload_of`]); the
/// latter only for a runner whose Hello has come (`hello`), or for a message that may come before it
/// (`Hello`, `Instance`, `Machine`, `Admit`). Where its piece said ([`Piece::of`]): on the shard, or on the compute pool, the
/// caller waiting either way, so its messages are still taken in order (goal 9, plan 01a1a87b2cec step 3: the hub's net loop read
/// runners' frames on its shard, holding it up to 76 ms).
pub async fn read_frame(data: Vec<u8>, hello: bool, piece: &Piece) -> Frame {
    read_by(piece, data, 0, hello).await
}

/// A runner's channel item (`crate::wire::channel::Frame::Item`), as [`read_item`] read it.
pub enum Item {
    /// The message it carries isn't one this build reads.
    Unreadable(crate::proto::Unreadable),
    /// The message it carries, what that carries read ([`payload_of`]), and the label of its kind for the
    /// shards' table of what held them (`crate::wire::agent_label`).
    Read { label: &'static hyper_rt::slow::Label, carried: Result<Carried<AgentMsg, FromRunner>, Box<Unreadable>> },
}

/// Reads the message a channel item carries, lying in its frame `data` from `at` (`crate::proto::item_at`), and what that
/// carries, as [`read_frame`] reads a frame: where its piece said, the caller waiting either way, so the items are still
/// taken in order (goal 9, plan 01a1a87b2cec step 3: the channel carries most of what a runner sends, its sessions' updates
/// among it, which the hub read on its shard whatever their length; the lab's s7c had its loop's polls under that label
/// held up to 28 ms). An item comes after its runner's Hello.
pub async fn read_item(data: Vec<u8>, at: usize, piece: &Piece) -> Item {
    match read_by(piece, data, at, true).await {
        Frame::Read { label, carried } => Item::Read { label, carried },
        Frame::Unreadable(e) => Item::Unreadable(e),
        // Read as after its Hello, so none comes before it; said as what it would be.
        Frame::BeforeHello { what } => Item::Unreadable(crate::proto::Unreadable::Malformed { variant: None, why: format!("{what}, before its runner's Hello") }),
    }
}

/// [`read_frame`] of the message lying in `data` from `at`, where `piece` said, learned by its kind's expectation. A read is
/// timed where it runs, so one off the shard teaches it too: else one slow sample (a preempted read) could send every frame
/// of a runner sending only long ones off the shard for good, none read on it to bring the expectation back down.
async fn read_by(piece: &Piece, data: Vec<u8>, at: usize, hello: bool) -> Frame {
    let len = u64::try_from(data.len().saturating_sub(at)).unwrap_or(u64::MAX);
    let (frame, took) = match piece.read {
        Read::Shard(_) => timed(|| read_now(data, at, hello)),
        Read::Pool => match crate::rt::compute(move || timed(|| read_now(data, at, hello))).await {
            Ok(read) => read,
            Err(e) => return Frame::Unreadable(crate::proto::Unreadable::Malformed { variant: None, why: format!("it couldn't be read off the shard: {e:#}") }),
        },
    };
    piece.costs.learn(len, took);
    frame
}

/// `read()`'s frame, and what it cost the thread that read it, nanoseconds: a read is computation on memory, so its time on
/// the CPU unless it's known it waited (`hyper_rt::futures::Spent::compute_cost_ns`). What its kind's costs learn, so a loaded
/// host's preemptions, which hold a shard whatever it runs, don't send short reads off it (goal 9, plan 01a1a87b2cec step
/// 5: on a host at load 75 the reads' wall times varied so that 37-55 µs reads went to the pool, their callers waiting
/// 218-363 µs for them).
fn timed(read: impl FnOnce() -> Frame) -> (Frame, u64) {
    let (frame, spent) = hyper_rt::futures::spent(read);
    (frame, spent.compute_cost_ns())
}

/// [`read_frame`]'s work, where it runs, of the message lying in `data` from `at`: an agent session's update where its JSON
/// lies ([`read_update`]), any other message as `crate::proto::read_agent` reads it.
fn read_now(data: Vec<u8>, at: usize, hello: bool) -> Frame {
    let message = data.get(at..).unwrap_or_default();
    if hello && let Some(update) = crate::proto::harness_at(message) {
        let (session, json) = (update.session.to_owned(), at.saturating_add(update.at));
        return Frame::Read { label: harness_label(), carried: read_update(session, data, json) };
    }
    match crate::proto::read_agent(message) {
        Err(e) => Frame::Unreadable(e),
        Ok(msg) if !hello && !matches!(msg, AgentMsg::Hello(_) | AgentMsg::Instance(_) | AgentMsg::Machine { .. } | AgentMsg::Admit) => Frame::BeforeHello { what: super::agent_name(&msg).to_owned() },
        Ok(msg) => Frame::Read { label: super::agent_label(&msg), carried: payload_of(msg) },
    }
}

/// The label of an agent session's update (`crate::wire::agent_label`'s, for `AgentMsg::Harness`).
fn harness_label() -> &'static hyper_rt::slow::Label {
    super::agent_label(&AgentMsg::Harness { session: String::new(), json: String::new() })
}

/// Agent session `session`'s update, its JSON read where it lies in its frame (`data` from `json`), its runner's stamp cut off
/// its end first (`crate::wire::stamp`): the update read as one without it, nothing moved or copied out of the frame (goal 9,
/// plan 01a1a87b2cec step 3: an item's body and then its JSON were each moved down over what came before them, and the text
/// read through for UTF-8 apart from its JSON).
fn read_update(session: String, mut data: Vec<u8>, json: usize) -> Result<Carried<AgentMsg, FromRunner>, Box<Unreadable>> {
    let (end, taken) = match data.get_mut(json..).and_then(super::stamp::cut) {
        Some((len, taken)) => (json.saturating_add(len), Some(taken)),
        None => (data.len(), None),
    };
    match payload_framed(Wire::RunnerPayload, "Harness", data, json..end) {
        Ok(update) => Ok(Carried::Read(FromRunner::Harness { session, update, taken })),
        Err(e) => Err(e.about(&session)),
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
    /// came: of it, the server reads only the fingerprint of the settings in use. Its text, read as
    /// JSON with no tree made ([`json_text`]): a tree of each, and the last one's compared with it
    /// key by key, held the hub's loop past 0.1 ms 4 times a second in train 16's lab, 0.13 ms on
    /// average (g9p16fp; goal 9, plan 01a1a87b2cec step 5).
    HeadroomStatus(String),
    /// `LegResults`: a leg's records, signed as the leg at its end (docs/ci.md W27-R05): Sigstore
    /// bundles, each checked as it's taken.
    LegResults { job: String, bundles: Vec<Value> },
    /// `LegSignature`: the provenance kiln wrote of what a leg built, signed as the leg (a Sigstore
    /// bundle), or why not (docs/ci.md W27-R06).
    LegSignature { job: String, nonce: String, bundle: Option<Value>, error: Option<String> },
    /// `Custody`: a custody record or delegation a runner process or a session it holds signed, or
    /// carried (a Sigstore bundle, docs/ci.md W27-R08), read where it's taken: by custody's intake, off
    /// the hub's loop, in the order its runner sent it ([`Later`]; `crate::custody::runners::Intake`).
    Custody(Later),
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
            AgentMsg::HeadroomStatus { json } => FromRunner::HeadroomStatus(json_text("HeadroomStatus", json)?),
            AgentMsg::LegResults { job, bundles } => FromRunner::LegResults { job, bundles: payload_owned(W, "LegResults", bundles)? },
            AgentMsg::LegSignature { job, nonce, bundle, error } => {
                let bundle = match bundle {
                    Some(b) => Some(payload_owned(W, "LegSignature", b)?),
                    None => None,
                };
                FromRunner::LegSignature { job, nonce, bundle, error }
            }
            // Read where it's taken, off the hub's loop ([`Later`]; goal 9, plan 01a1a87b2cec step 3).
            AgentMsg::Custody { json } => FromRunner::Custody(Later { of: "Custody", json }),
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
            | AgentMsg::Admit
            | AgentMsg::Settled
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
            | AgentMsg::LinkUnread { .. }
            // A fenced message's own is read as it's taken (`crate::hub`), under its grant.
            | AgentMsg::Renew { .. }
            | AgentMsg::Refused { .. }
            | AgentMsg::Fenced { .. }
            | AgentMsg::Features { .. }
            // The offers hint's JSON is read by its view (lb/offers-ui), which keeps what it can.
            | AgentMsg::Offers { .. }
            | AgentMsg::Placed { .. }
            // Installed skills' JSON is read by `crate::skills`, which keeps what it can.
            | AgentMsg::Skills { .. }
            | AgentMsg::SkillFile { .. }) => return Ok(Carried::Nothing(m)),
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
            | ServerMsg::HarnessArchive { .. }
            | ServerMsg::HarnessRestoreCopy { .. }
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
            | ServerMsg::SignAsLeg { .. }
            | ServerMsg::Renewed { .. }
            | ServerMsg::Stop { .. }
            | ServerMsg::Features { .. }
            | ServerMsg::Offers { .. }
            | ServerMsg::Place { .. }
            | ServerMsg::Copies { .. }
            | ServerMsg::Skills { .. }
            | ServerMsg::SkillFile { .. }
            | ServerMsg::Later { .. }
            | ServerMsg::Back { .. }) => return Ok(Carried::Nothing(m)),
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

    /// Goal 9, plan 01a1a87b2cec step 5: what a read teaches is its cost to the thread ([`timed`]: its CPU time unless it
    /// waited inside a call). Do: one kind of read learned from reads a loaded host preempted (wall 300 µs, 40 µs of it on the
    /// CPU, no wait of its own), another from reads that waited inside a call as long. Expect, against a quantum of 266.7 µs
    /// and a pool round trip of 200 µs: the preempted kind read on the shard, at its CPU time; the waiting kind off it.
    /// Weighed by their wall times, as before, both went off.
    #[test]
    fn g9_s5_a_read_is_weighed_by_its_cpu_time_unless_it_waited() {
        use hyper_rt::futures::Spent;
        let sides = crate::rt::Sides { quantum_ns: Some(crate::rt::STEP_BUDGET_NS), trip_ns: Some(200_000) };
        let (preempted, waited, by_wall) = (Costs::nothing_yet(), Costs::nothing_yet(), Costs::nothing_yet());
        let read = |cpu: u64, blocked: bool| Spent { wall_ns: 300_000, cpu_ns: Some(cpu), blocked: Some(blocked) };
        for _ in 0..32 {
            preempted.learn(1_000, read(40_000, false).compute_cost_ns());
            waited.learn(1_000, read(40_000, true).compute_cost_ns());
            by_wall.learn(1_000, read(40_000, false).wall_ns);
        }
        assert_eq!(on_shard(&preempted, 1_000, sides), Some(40_000), "preempted: on the shard, at its CPU time");
        assert_eq!(on_shard(&waited, 1_000, sides), None, "it waited: off the shard");
        assert_eq!(on_shard(&by_wall, 1_000, sides), None, "by its wall time, as before: off the shard");
    }
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
                    ServerMsg::Wire(Frame::Ack(has)) if &*has.stream == "s:s1#a1" => has.nacks.iter().find(|n| n.seq == seq).map(|n| n.code),
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
            static EXPECT: Costs = Costs::nothing_yet();
            static REST: Costs = Costs::nothing_yet();
            EXPECT.learn(len, 1_000_000_000);
            anyhow::ensure!(on_shard(&EXPECT, len, crate::rt::Sides::here()).is_none(), "expected slow: a second's read costs more than the pool's round trip");
            for _ in 0..64 {
                let frame = read_frame(data.clone(), true, &Piece::placed(&data, &EXPECT, &REST, crate::rt::Sides::here())).await;
                anyhow::ensure!(matches!(frame, Frame::Read { carried: Ok(_), .. }), "the frame is read");
            }
            // Each read taught its own cost, and a window on (the 59th sample: `hyper_rt::machine::bench::MIN_SAMPLES`) the
            // sketch halved, the second's read weighing half: about 15 ms of it is left a read, plus what the reads took; a
            // hundred milliseconds leaves a debug build's reads plenty of room.
            let left = expected(&EXPECT, len);
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
            async fn read(b: Vec<u8>) -> Item {
                let piece = Piece::of(&b);
                read_item(b, 0, &piece).await
            }
            let term = crate::proto::encode(&AgentMsg::TermOutput { session: "s1".into(), data: vec![1, 2, 3] })?;
            match read(term).await {
                Item::Read { label, carried: Ok(Carried::Nothing(AgentMsg::TermOutput { .. })) } => anyhow::ensure!(label.name.contains("TermOutput"), "labeled {}", label.name),
                _ => anyhow::bail!("not read as the message it carries"),
            }
            let nested = crate::proto::encode(&AgentMsg::Wire(crate::wire::channel::Frame::Want { stream: "s".into(), from: 1 }))?;
            anyhow::ensure!(matches!(read(nested).await, Item::Read { carried: Ok(Carried::Nothing(AgentMsg::Wire(_))), .. }), "a channel frame inside one comes back as itself");
            anyhow::ensure!(matches!(read(vec![0xff; 4]).await, Item::Unreadable(_)), "bytes that aren't a message");
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
        anyhow::ensure!(!std::ptr::eq(kind_costs(&term("s")?).0, kind_costs(&harness).0), "each kind its own");
        anyhow::ensure!(std::ptr::eq(kind_costs(&term("s")?).0, kind_costs(&term("t")?).0), "a kind's own, each time");
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
            // One payload of a kind is looked at at a time ([`looks_at`]): the one with `new`, read again on a shard once the
            // last look is done, is.
            until_looked(wire, "Two").await?;
            let newer = CASES.last().copied().unwrap_or_default();
            anyhow::ensure!(payload::<Two>(wire, "Two", newer).is_ok(), "{newer} wasn't read");
            // `new` came with something in it (`empty` didn't): counted once the pool's spare time has looked.
            let deadline = Instant::now() + Duration::from_secs(30);
            while crate::metrics::wire().ignored.get(&[wire.name()]).saturating_sub(before) < 1 {
                anyhow::ensure!(Instant::now() < deadline, "the unknown field read on a shard wasn't counted");
                spare_turn().await?;
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
        let harness = |u: &Update| -> anyhow::Result<Vec<u8>> { crate::proto::encode(&AgentMsg::Harness { session: "s1".into(), json: serde_json::to_string(u)? }) };
        let events = Update::Events { events: Vec::new() };
        let patch = Update::ChangesPatch { set: Vec::new(), gone: Vec::new() };
        for (u, tag) in [(&Update::Closed, "closed"), (&Update::Reset { from: Some(3) }, "reset"), (&events, "events"), (&patch, "changes_patch")] {
            let json = serde_json::to_string(u)?;
            anyhow::ensure!(update_kind(json.as_bytes()) == UPDATE_TAGS.iter().position(|t| *t == tag), "{json}: its kind");
            let data = harness(u)?;
            anyhow::ensure!(UPDATE_TAGS.iter().position(|t| *t == tag).and_then(|k| UPDATE_COSTS.get(k)).is_some_and(|e| std::ptr::eq(kind_costs(&data).0, e)), "{json}: its own expectation");
        }
        anyhow::ensure!(!std::ptr::eq(kind_costs(&harness(&events)?).0, kind_costs(&harness(&patch)?).0), "each kind its own");
        // Its tag elsewhere, or a kind this build doesn't know: runners' updates' expectation, as before.
        let whole = KIND_COSTS.get(kind_of(&harness(&events)?)).unwrap_or(&REST_COSTS);
        for said in [r#"{"events":[],"t":"events"}"#, r#"{"t":"teleport"}"#, r#"{ "t":"events","events":[]}"#, "", "{"] {
            let data = crate::proto::encode(&AgentMsg::Harness { session: "s1".into(), json: said.into() })?;
            anyhow::ensure!(std::ptr::eq(kind_costs(&data).0, whole), "{said}: runners' updates' expectation");
        }
        let term = crate::proto::encode(&AgentMsg::TermOutput { session: "s1".into(), data: vec![1; 8] })?;
        anyhow::ensure!(std::ptr::eq(kind_costs(&term).0, KIND_COSTS.get(kind_of(&term)).unwrap_or(&REST_COSTS)), "another message its kind's");
        Ok(())
    }

    /// Goal 9, plan 01a1a87b2cec step 3: a frame's piece is the message it holds, a channel item's the one it carries, whose
    /// head adds nothing to place: learned by that message's kind. Its read is on the shard, expected to take its bytes at the
    /// rate learned, where that costs no more than the pool's round trip, and off it where it costs more; nothing learned of
    /// reads like it, off it, which teaches them.
    #[test]
    fn g9_s3_a_frames_piece_is_the_message_it_holds() -> anyhow::Result<()> {
        use crate::wire::channel::Frame;
        let body = crate::proto::encode(&AgentMsg::Harness { session: "s1".into(), json: r#"{"t":"closed"}"#.into() })?;
        let item = crate::proto::encode(&AgentMsg::Wire(Frame::Item { stream: "s:s1#a1".into(), seq: 2, low: 1, body: body.clone() }))?;
        let (of_item, of_body) = (Piece::of(&item), Piece::of(&body));
        anyhow::ensure!(std::ptr::eq(of_item.costs, of_body.costs) && std::ptr::eq(of_item.rest, of_body.rest), "an item's piece: its message's");
        let len = u64::try_from(body.len())?;
        // A shard's quantum and the pool's round trip a millisecond (consts-audit B1): reads of a few hundred nanoseconds fit
        // the one and cost less than the other.
        let sides = crate::rt::Sides { quantum_ns: Some(266_666), trip_ns: Some(1_000_000) };
        static MESSAGES: Costs = Costs::nothing_yet();
        static SLOW: Costs = Costs::nothing_yet();
        static REST: Costs = Costs::nothing_yet();
        anyhow::ensure!(Piece::placed(&body, &MESSAGES, &REST, sides).read == Read::Pool, "nothing learned: off the shard");
        MESSAGES.learn(1_000, 20_000);
        anyhow::ensure!(Piece::placed(&body, &MESSAGES, &REST, sides).read == Read::Shard(20 * len), "20 ns a byte, on the shard");
        // A millisecond a byte, dearer than the round trip: off the shard.
        SLOW.learn(1, 1_000_000);
        anyhow::ensure!(Piece::placed(&body, &SLOW, &REST, sides).read == Read::Pool, "dearer than the round trip: the pool");
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
            // One payload of a kind is looked at at a time ([`looks_at`]): the one with `handed`, handed over again on a shard
            // once the last look is done, is.
            until_looked(wire, "Two").await?;
            let newer = CASES.last().copied().unwrap_or_default();
            anyhow::ensure!(payload_owned::<Two>(wire, "Two", newer.to_owned()).is_ok(), "{newer} wasn't read");
            // `handed` came with something in it (`empty` didn't): counted once the pool's spare time has looked.
            let deadline = Instant::now() + Duration::from_secs(30);
            while crate::metrics::wire().ignored.get(&[wire.name()]).saturating_sub(before) < 1 {
                anyhow::ensure!(Instant::now() < deadline, "the unknown field of a payload handed over wasn't counted");
                spare_turn().await?;
                crate::rt::sleep(Duration::from_millis(5)).await;
            }
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 3; consts-audit B1: a read with nothing learned of reads like it goes off the shard;
    /// once one is, a read goes on the shard where it's expected to cost no more than the compute pool's round trip and the
    /// quantum (`kiln_http::turn::Costs::fit`, nothing free), off it past either. Do: reads learned at 12.5 ns a byte; weigh
    /// 6,000 bytes against a fast pool (a 50 µs round trip) and a slow one (200 µs), 4,000 against both, 22,000 against a
    /// loaded host's (10 ms), and any against a pool nothing's been learned of. Expect: 75 µs goes to the fast pool and stays
    /// against the slow one; 50 µs stays against both; 275 µs, past the quantum, goes to the pool however long its round
    /// trip; with nothing learned of the pool, a read goes there, which teaches it.
    #[test]
    fn g9_s3_b1_a_read_goes_on_the_shard_within_the_quantum_where_its_no_dearer_than_the_pool() -> anyhow::Result<()> {
        use crate::rt::Sides;
        const QUANTUM: Option<u64> = Some(266_666);
        let (fast, slow, loaded) = (Sides { quantum_ns: QUANTUM, trip_ns: Some(50_000) }, Sides { quantum_ns: QUANTUM, trip_ns: Some(200_000) }, Sides { quantum_ns: QUANTUM, trip_ns: Some(10_000_000) });
        let costs = Costs::nothing_yet();
        anyhow::ensure!(on_shard(&costs, 1, slow).is_none(), "nothing learned: off the shard");
        costs.learn(1_000, 12_500);
        anyhow::ensure!(on_shard(&costs, 6_000, fast).is_none(), "75 µs against a 50 µs round trip: the pool");
        anyhow::ensure!(on_shard(&costs, 6_000, slow) == Some(75_000), "75 µs against a 200 µs round trip: the shard");
        anyhow::ensure!(on_shard(&costs, 4_000, fast) == Some(50_000) && on_shard(&costs, 4_000, slow) == Some(50_000), "4,000 bytes");
        anyhow::ensure!(on_shard(&costs, 22_000, loaded).is_none(), "275 µs, past the quantum, against a 10 ms round trip: the pool");
        anyhow::ensure!(on_shard(&costs, 16_000, loaded) == Some(200_000), "200 µs against a 10 ms round trip: the shard");
        anyhow::ensure!(on_shard(&costs, 1_000, Sides { quantum_ns: QUANTUM, trip_ns: None }).is_none(), "nothing learned of the pool: there, which teaches it");
        anyhow::ensure!(on_shard(&costs, 1_000_000, Sides { quantum_ns: None, trip_ns: None }) == Some(12_500_000), "off a shard: where it's asked");
        Ok(())
    }

    /// Goal 9, plan 01a1a87b2cec step 3: the first of each kind of runner message is read off the shard, in order, whatever
    /// its length, and teaches its kind what reading one takes, so the next like it is read on the shard.
    #[test]
    fn g9_s3_the_first_read_of_a_kind_goes_off_the_shard_and_teaches_it() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let data = crate::proto::encode(&AgentMsg::TermOutput { session: "s1".into(), data: vec![7; 64] })?;
            let len = u64::try_from(data.len())?;
            static EXPECT: Costs = Costs::nothing_yet();
            static REST: Costs = Costs::nothing_yet();
            let piece = Piece::placed(&data, &EXPECT, &REST, crate::rt::Sides::here());
            anyhow::ensure!(piece.read == Read::Pool, "nothing learned yet: off the shard");
            let frame = read_frame(data.clone(), true, &piece).await;
            anyhow::ensure!(matches!(frame, Frame::Read { carried: Ok(Carried::Nothing(AgentMsg::TermOutput { .. })), .. }), "the frame is read whole off the shard");
            anyhow::ensure!(EXPECT.mean().is_some() && len > 0, "and teaches its kind");
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 3: a piece of the hub's loop's work fits what's left of its turn where its read on the
    /// shard, as expected, and what its kind takes past it at the dearest it has been fit (learned of the first poll of each
    /// piece of its kind); one of a kind nothing's been learned of fits no room, so it's its turn's first. A channel item's
    /// piece is the message it carries', an agent session's update's its kind of update's; and what a piece does after it
    /// first waits isn't its kind's to learn (it isn't in the poll the piece started in).
    #[test]
    fn g9_s3_a_piece_fits_its_read_and_what_its_kind_takes_past_it() -> anyhow::Result<()> {
        use crate::wire::channel::Frame as Channel;
        // Which kind a frame's piece is kept by.
        let state = crate::harness::core::Update::State { state: Box::default() };
        let update = crate::proto::encode(&AgentMsg::Harness { session: "s1".into(), json: serde_json::to_string(&state)? })?;
        let item = crate::proto::encode(&AgentMsg::Wire(Channel::Item { stream: "s:s1#a1".into(), seq: 1, low: 0, body: update.clone() }))?;
        let custody = crate::proto::encode(&AgentMsg::Custody { json: "{}".into() })?;
        let state_kind = UPDATE_TAGS.iter().position(|t| *t == "state").and_then(|k| UPDATE_PIECES.get(k));
        anyhow::ensure!(state_kind.is_some_and(|s| std::ptr::eq(Piece::of(&update).rest, s)), "an update: its kind of update's");
        anyhow::ensure!(std::ptr::eq(Piece::of(&item).rest, Piece::of(&update).rest), "a channel item: the message it carries'");
        anyhow::ensure!(PIECES.get(kind_of(&custody)).is_some_and(|s| std::ptr::eq(Piece::of(&custody).rest, s)), "a message: its kind's");
        anyhow::ensure!(std::ptr::eq(Piece::job_log().rest, &LOG_PIECES), "job output: its own");
        static FRESH: Costs = Costs::nothing_yet();
        let piece = Piece { read: Read::Shard(20_000), costs: &REST_COSTS, rest: &FRESH };
        anyhow::ensure!(!piece.fits(i64::MAX), "nothing learned of its kind: no room is enough");
        // A frame whose read goes off the shard (nothing learned of its kind's reads, the first of them): its first poll ends
        // there, before its handler, and teaches its kind nothing.
        static UNTAUGHT: Costs = Costs::nothing_yet();
        crate::rt::block_on(async { anyhow::Ok(Piece::take(Some(Piece { read: Read::Pool, costs: &REST_COSTS, rest: &UNTAUGHT }), async { 7 }).await) })?;
        anyhow::ensure!(UNTAUGHT.mean().is_none(), "a piece read off the shard taught its kind");
        crate::rt::block_on(async move {
            // A piece that waits once, its first poll spun for 200 µs: its kind learns what that poll took past its read's
            // 20 µs, once; its second poll, after the wait, isn't learned.
            let mut polls = 0;
            let work = std::future::poll_fn(move |cx| {
                polls += 1;
                if polls == 1 {
                    // Spun till 200 µs went by on the clock a piece is timed by (its thread's on the CPU where it says, else
                    // the wall's), a second of the wall's at the most.
                    let (cpu, wall) = (hyper_rt::futures::thread_run_ns(), Instant::now());
                    while wall.elapsed() < Duration::from_secs(1) {
                        let spun = match (cpu, hyper_rt::futures::thread_run_ns()) {
                            (Some(from), Some(to)) => to.saturating_sub(from),
                            _ => u64::try_from(wall.elapsed().as_nanos()).unwrap_or(u64::MAX),
                        };
                        if spun >= 200_000 {
                            break;
                        }
                        std::hint::spin_loop();
                    }
                    cx.waker().wake_by_ref();
                    return std::task::Poll::Pending;
                }
                std::task::Poll::Ready(7)
            });
            anyhow::ensure!(Piece::take(Some(piece), work).await == 7, "its work's output");
            let learned = FRESH.mean().map_or(0, |ns| ns as i64);
            anyhow::ensure!(learned >= 180_000, "its first poll past its read, once: {learned} ns");
            // Its read and what its kind took past it fit a room of both, at the dearest that was (a sketch's bucket's own,
            // within the power of two it fell in), and no less.
            anyhow::ensure!(piece.fits(20_000 + 2 * learned) && !piece.fits(20_000 + learned / 2), "fits its read and twice what's past it, not half");
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 3: what a kind's pieces take past their reads is weighed at the dearest they have been,
    /// forgotten only as the sketch's windows halve it (`kiln_http::turn::Costs`), not at a smoothed mean. Do: a kind whose
    /// pieces take 10 µs past their reads, the second of them 200 µs (a runner's record copied once a second, say), then 40
    /// at 10 µs; ask whether the next fits 60 µs of room, and 250 µs. Expect: not 60 µs, 250 µs yes. RFC 6298's bound (the
    /// mean and four variations, gains 1/8 and 1/4) had forgotten the dear one by then, 10 µs and four times a variation of
    /// about 47.5 × (3/4)^40 µs: the next was taken into 60 µs it didn't fit.
    #[test]
    fn g9_s3_a_kinds_rare_dear_piece_is_room_its_turn_keeps() {
        static PAST: Costs = Costs::nothing_yet();
        let piece = Piece { read: Read::Shard(0), costs: &REST_COSTS, rest: &PAST };
        PAST.learn(1, 10_000);
        PAST.learn(1, 200_000);
        for _ in 0..40 {
            PAST.learn(1, 10_000);
        }
        assert!(!piece.fits(60_000), "200 µs, its dearest: not into 60 µs");
        assert!(piece.fits(250_000), "into 250 µs");
    }

    /// Goal 9, plan 01a1a87b2cec step 3: a runner's custody record comes through the hub's loop unread, its JSON as it came,
    /// for custody's intake to read where it takes it: there it reads as any JSON value it carries does, and what isn't JSON
    /// is refused as `malformed`, saying why, as where its frame was read.
    #[test]
    fn g9_s3_a_custody_record_is_read_where_its_taken() -> anyhow::Result<()> {
        for (json, good) in [(r#"{"refused_hop":{"what":"a run"}}"#, true), ("{", false), ("not json", false)] {
            let Ok(Carried::Read(FromRunner::Custody(record))) = payload_of(AgentMsg::Custody { json: json.into() }) else {
                anyhow::bail!("{json}: not passed on unread");
            };
            anyhow::ensure!(record.bytes() >= json.len(), "{json}: its text held");
            match record.read() {
                Ok(v) => anyhow::ensure!(good && v.pointer("/refused_hop/what").and_then(Value::as_str) == Some("a run"), "{json}: read as {v}"),
                Err(e) => anyhow::ensure!(!good && e.of == "Custody" && e.code == Code::Malformed && !e.why.is_empty(), "{json}: refused as {e:?}"),
            }
        }
        Ok(())
    }

    /// A job for the compute pool: its worker, free once it's done, takes the payloads waiting to be looked at
    /// ([`check_spare`]) before it rests.
    async fn spare_turn() -> anyhow::Result<()> {
        crate::rt::compute(|| ()).await
    }

    /// Until the look at a payload of variant `of` on `wire` under way is done, the pool given turns meanwhile: 30 s at the
    /// most.
    async fn until_looked(wire: Wire, of: &str) -> anyhow::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(30);
        while looking(wire, of) {
            anyhow::ensure!(Instant::now() < deadline, "the look at a {of} on {} wasn't done in 30 s", wire.name());
            spare_turn().await?;
            crate::rt::sleep(Duration::from_millis(1)).await;
        }
        Ok(())
    }

    /// Consts-audit R3 (goal 9, plan 01a1a87b2cec step 5): a payload handed over to be looked at wakes no thread and is looked
    /// at with the compute pool's spare time: no thread waits on what's waiting (each wait there would be woken by a payload
    /// handed over, a futex's wake on the hub's shard). Do: hand payloads over, each with a field the type doesn't have; then
    /// give the pool jobs. Expect: no wait listed as each is handed over; each counted once a worker is free.
    #[test]
    fn g9_s5_r3_a_payload_handed_over_wakes_no_thread_and_is_looked_at_with_spare_time() -> anyhow::Result<()> {
        #[derive(Debug, PartialEq, Serialize, serde::Deserialize)]
        struct One {
            a: u32,
        }
        let Some((tx, _)) = checks() else { anyhow::bail!("no spare time to look with") };
        // A wire no other test reads JSON on, so the count is this test's alone.
        let wire = Wire::EventsDown;
        let before = crate::metrics::wire().ignored.get(&[wire.name()]);
        const HANDED: u64 = 6;
        for n in 0..HANDED {
            anyhow::ensure!(tx.takers_listed() == 0, "a thread waits on what's waiting: payload {n} handed over would wake it");
            check_owned(wire, "One", format!(r#"{{"a":{n},"looked_at":true}}"#), unknown_in::<One>, Looking(None));
        }
        crate::rt::block_on(async move {
            let deadline = Instant::now() + Duration::from_secs(30);
            while crate::metrics::wire().ignored.get(&[wire.name()]).saturating_sub(before) < HANDED {
                anyhow::ensure!(Instant::now() < deadline, "the payloads handed over weren't all looked at");
                spare_turn().await?;
                crate::rt::sleep(Duration::from_millis(5)).await;
            }
            anyhow::Ok(())
        })?;
        anyhow::ensure!(tx.takers_listed() == 0, "a thread waits on what's waiting");
        Ok(())
    }

    /// A payload type with one field, for the looks' tests.
    #[derive(Debug, PartialEq, Serialize, serde::Deserialize)]
    struct OneField {
        a: u32,
    }

    /// Consts-audit R3 (goal 9, plan 01a1a87b2cec step 5): one payload of a kind is looked at at a time, the next once the
    /// last is done, with no time between (it was one of a kind every 100 ms). Do: claim a kind's look; read payloads of it,
    /// each with a field the type doesn't have, on a shard; let the look go; read one more; then read payloads off a shard,
    /// where each is looked at as it's read. Expect: none looked at while the look is under way, each read straight; the one
    /// after it looked at; off a shard, a checker free at once, every one looked at.
    #[test]
    fn g9_s5_r3_one_payload_of_a_kind_is_looked_at_at_a_time_the_next_once_its_done() -> anyhow::Result<()> {
        // A wire no other test reads JSON on, so its counts are this test's alone.
        let wire = Wire::BpDown;
        let checked = move || crate::metrics::wire().checked.get(&[wire.name()]);
        crate::rt::block_on(async move {
            let held = looks_at(wire, "Held").ok_or_else(|| anyhow::anyhow!("the kind's look was under way already"))?;
            anyhow::ensure!(looks_at(wire, "Held").is_none() && looking(wire, "Held"), "a second look of a kind while one is under way");
            let looked = checked();
            for n in 0..16u32 {
                let read = payload::<OneField>(wire, "Held", &format!(r#"{{"a":{n},"new":true}}"#)).map_err(|e| anyhow::anyhow!("{e}"))?;
                anyhow::ensure!(read == OneField { a: n }, "read as {read:?}");
            }
            anyhow::ensure!(checked() == looked, "looked at while its kind's look was under way");
            drop(held);
            anyhow::ensure!(!looking(wire, "Held"), "its look let go");
            payload::<OneField>(wire, "Held", r#"{"a":16,"new":true}"#).map_err(|e| anyhow::anyhow!("{e}"))?;
            until_looked(wire, "Held").await?;
            anyhow::ensure!(checked() == looked + 1, "the next after it, looked at: {} looked at", checked().saturating_sub(looked));
            anyhow::Ok(())
        })?;
        // Off a shard, each is looked at as it's read: its look done before the next comes.
        let looked = checked();
        for n in 0..16u32 {
            payload::<OneField>(wire, "Free", &format!(r#"{{"a":{n},"new":true}}"#)).map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        anyhow::ensure!(checked().saturating_sub(looked) == 16, "{} of 16 looked at by a checker free at once", checked().saturating_sub(looked));
        Ok(())
    }

    /// No handler reads JSON of its own: in the hub, the runner and the harness's host, a read of
    /// what came on a wire is [`payload`]'s, and every other read is of something of the machine's
    /// own (a file, a socket of its own, what it's about to send), each named here.
    #[test]
    fn no_handler_reads_a_payload_of_its_own() {
        const OWN: [(&str, &str); 10] = [
            ("serde_json::from_slice(&json).ok()?", "a session handed back by this machine's newer build, on a socket of its own"),
            ("serde_json::from_value(cut_value(v, budget))", "its own event, cut to fit a frame before it's sent"),
            ("let handed: Vec<HandedTerm> = match serde_json::from_slice(&raw)", "terminals the runner before it handed over, in a file"),
            ("Ok(raw) => serde_json::from_slice(&raw).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),", "its records of hand-overs, in files of its own"),
            ("serde_json::from_reader(std::io::BufReader::new(archive_file))", "mc_attention: its private local archive proof and retained conversation (01a199e1b177752e84dad5dc990e6a15)"),
            ("serde_json::from_slice(&raw).unwrap_or_else(|e| {", "the sessions it handed itself, and its record of its sessions, in its files"),
            ("let state: super::model::State = kept.state.as_deref().and_then(|s| serde_json::from_str(s).ok())?;", "what its conversation store kept"),
            ("let mut events: Vec<super::model::Event> = kept.events.iter().filter_map(|e| serde_json::from_str(&e.body).ok()).collect();", "what its conversation store kept"),
            ("Some((k.strip_prefix(\"topo/link/\")?.to_owned(), serde_json::from_str(&v).ok()?))", "the fleet's link history its store kept (LB08)"),
            ("self.store.kv_get(\"topo/codrops\".to_owned()).await.ok().flatten().and_then(|v| serde_json::from_str(&v).ok())", "the fleet's co-drops its store kept (LB08)"),
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
    /// Goal 9, plan 01a1a87b2cec step 3: a frame's read is done where its piece placed it, whatever its kind has learned
    /// since. A piece placed on the pool (nothing learned of its kind's reads) is read there, though its kind has learned
    /// meanwhile that reads like it are cheap; one placed on the shard is read there. The loop counts a frame's turn by its
    /// piece (`crate::rt::Turns::room_for`): decided again as the read was done, a read its piece placed on the pool ran on
    /// the shard, a read its turn never counted.
    #[test]
    fn g9_s3_a_read_is_done_where_its_piece_placed_it() -> anyhow::Result<()> {
        static COSTS: Costs = Costs::nothing_yet();
        static REST: Costs = Costs::nothing_yet();
        crate::rt::block_on(async {
            let data = crate::proto::encode(&AgentMsg::TermOutput { session: "s1".into(), data: vec![7; 512] })?;
            let len = u64::try_from(data.len())?;
            let sides = crate::rt::Sides { quantum_ns: Some(266_666), trip_ns: Some(1_000_000) };
            let pool = Piece::placed(&data, &COSTS, &REST, sides);
            anyhow::ensure!(pool.read == Read::Pool, "nothing learned: the pool");
            // Other loops teach its kind meanwhile: a read like it takes 10 µs, cheaper than the pool's round trip.
            COSTS.learn(len, 10_000);
            anyhow::ensure!(Piece::placed(&data, &COSTS, &REST, sides).read != Read::Pool, "placed now, it would be on the shard");
            // The compute pool's jobs a read's own polls ask for (the count is its thread's: other tasks on the shard ask too
            // while the read waits).
            async fn asked<F: std::future::Future>(read: F) -> (F::Output, u64) {
                let mut read = std::pin::pin!(read);
                let mut asked = 0;
                let out = std::future::poll_fn(|cx| {
                    let before = crate::rt::COMPUTE_ASKED.with(std::cell::Cell::get);
                    let polled = read.as_mut().poll(cx);
                    asked += crate::rt::COMPUTE_ASKED.with(std::cell::Cell::get).wrapping_sub(before);
                    polled
                })
                .await;
                (out, asked)
            }
            let (frame, jobs) = asked(read_frame(data.clone(), true, &pool)).await;
            anyhow::ensure!(matches!(frame, Frame::Read { carried: Ok(_), .. }) && jobs == 1, "read on the pool, as placed: {jobs} jobs");
            let shard = Piece { read: Read::Shard(0), costs: &COSTS, rest: &REST };
            let (frame, jobs) = asked(read_frame(data, true, &shard)).await;
            anyhow::ensure!(matches!(frame, Frame::Read { carried: Ok(_), .. }) && jobs == 0, "read on the shard, as placed: {jobs} jobs");
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 3: a read is placed on the shard only where it and what its piece takes past it (its
    /// kind's dearest) fit the quantum together, so no frame alone holds a poll past it: a read whose bound alone fit went on
    /// the shard, and its handler took the poll past the quantum.
    #[test]
    fn g9_s3_a_read_on_the_shard_fits_the_quantum_with_what_its_piece_takes_past_it() -> anyhow::Result<()> {
        let sides = crate::rt::Sides { quantum_ns: Some(266_666), trip_ns: Some(1_000_000) };
        static COSTS: Costs = Costs::nothing_yet();
        static HANDLED: Costs = Costs::nothing_yet();
        let message = vec![0u8; 1_000];
        // 1,000 bytes in 200 µs: it alone fits the quantum, and nothing's learned past it.
        COSTS.learn(1_000, 200_000);
        anyhow::ensure!(Piece::placed(&message, &COSTS, &HANDLED, sides).read == Read::Shard(200_000), "nothing learned past its read: the shard");
        // Its handler took 80 µs past its read: 200 and 80 µs pass the quantum together.
        HANDLED.learn(1, 80_000);
        anyhow::ensure!(Piece::placed(&message, &COSTS, &HANDLED, sides).read == Read::Pool, "200 µs and 80 µs past it: the pool");
        anyhow::ensure!(Piece::placed(message.get(..500).unwrap_or_default(), &COSTS, &HANDLED, sides).read == Read::Shard(100_000), "100 µs and 80 µs: the shard");
        Ok(())
    }

    /// Goal 9, plan 01a1a87b2cec step 3: a channel item's head is read where it lies (`crate::proto::item_at`), and the message
    /// it carries, or a frame's own, from where it lies in the frame. An agent session's update read so, at each length where
    /// a varint grows, is the update the whole message says, its runner's stamp cut off its end. Any other frame, and what
    /// isn't read so (bytes left over, cut short, a test build's mark), reads or is refused as `crate::proto::read_agent` reads
    /// or refuses it; an update's text that isn't UTF-8 is refused.
    #[test]
    fn g9_s3_a_frames_message_is_read_where_it_lies() -> anyhow::Result<()> {
        use crate::harness::core::Update;
        use crate::wire::channel::Frame as Channel;
        for len in [0_usize, 1, 127, 128, 16_383, 16_384] {
            let text: String = (0..len).map(|i| char::from(b'a'.saturating_add(u8::try_from(i % 26).unwrap_or(0)))).collect();
            let update = Update::Stream { id: "e1".into(), text };
            let mut json = serde_json::to_string(&update)?;
            crate::wire::stamp::put(&mut json, Duration::from_micros(250));
            let body = crate::proto::encode(&AgentMsg::Harness { session: format!("s{len}"), json })?;
            let item = crate::proto::encode(&AgentMsg::Wire(Channel::Item { stream: format!("s:s1#{len}"), seq: 9, low: 3, body: body.clone() }))?;
            let head = crate::proto::item_at(&item).ok_or_else(|| anyhow::anyhow!("{len}: no item's head"))?;
            match crate::proto::read_agent(&item) {
                Ok(AgentMsg::Wire(Channel::Item { stream, seq, low, body: whole })) => {
                    anyhow::ensure!(head.stream == stream && head.seq == seq && head.low == low && item.get(head.at..) == Some(whole.as_slice()), "{len}: its head as the whole item says it");
                }
                _ => anyhow::bail!("{len}: not read as an item"),
            }
            for (frame, at) in [(body.clone(), 0), (item.clone(), head.at)] {
                match read_now(frame, at, true) {
                    Frame::Read { carried: Ok(Carried::Read(FromRunner::Harness { session, update: read, taken })), .. } => {
                        anyhow::ensure!(session == format!("s{len}") && *read == update && taken == Some(Duration::from_micros(250)), "{len}: read as {read:?}, {taken:?}");
                    }
                    _ => anyhow::bail!("{len}: not read as its update"),
                }
            }
        }
        let update = crate::proto::encode(&AgentMsg::Harness { session: "s1".into(), json: r#"{"t":"stream","id":"e1","text":"ab"}"#.into() })?;
        let item = crate::proto::encode(&AgentMsg::Wire(Channel::Item { stream: "s:s1#a1".into(), seq: 1, low: 0, body: update.clone() }))?;
        let mut odd: Vec<Vec<u8>> = Vec::new();
        for m in [&update, &item] {
            odd.push([m.as_slice(), &[0]].concat());
            odd.push(m.get(..m.len().saturating_sub(1)).unwrap_or_default().to_vec());
            odd.push([crate::net::MARK.as_slice(), m].concat());
        }
        odd.extend([crate::proto::encode(&AgentMsg::TermOutput { session: "s1".into(), data: vec![1, 2] })?, crate::proto::encode(&AgentMsg::Caps { caps: crate::proto::CAP_WIRE })?]);
        for b in odd {
            anyhow::ensure!(crate::proto::item_at(&b).is_none(), "{b:?}: read as an item's head");
            match (read_now(b.clone(), 0, true), crate::proto::read_agent(&b)) {
                (Frame::Unreadable(mine), Err(whole)) => anyhow::ensure!(mine == whole, "{b:?}: refused as {mine:?}, whole as {whole:?}"),
                (Frame::Read { label, .. }, Ok(whole)) => anyhow::ensure!(std::ptr::eq(label, crate::wire::agent_label(&whole)), "{b:?}: read as another message"),
                _ => anyhow::bail!("{b:?}: read where read_agent refuses it, or refused where it reads it"),
            }
        }
        let not_utf8: Vec<u8> = update.iter().map(|b| if *b == b'b' { 0xff } else { *b }).collect();
        anyhow::ensure!(matches!(read_now(not_utf8, 0, true), Frame::Read { carried: Err(e), .. } if e.code == Code::Malformed), "text that isn't UTF-8 is refused");
        Ok(())
    }
}
