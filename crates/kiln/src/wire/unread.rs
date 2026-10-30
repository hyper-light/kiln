//! What can't be read, on any wire (docs/wire.md, "What can't be read"). One record, [`Unread`], for
//! any message a wire refuses because it can't read it, and one place each is noted,
//! [`Refusals::note`] ([`Refusals::note_told`] for one its sender is told of). A refusal is:
//! - counted, in `kiln_wire_refused_total{wire,code}`, with no label for its sender;
//! - kept for its sender's page: each runner's and agent session's counts by wire and code, and its
//!   newest few;
//! - written to the audit log as `security.wire_refused`, coalesced: the first for a sender, wire and
//!   code in a minute at once, the rest in one row a minute saying how many;
//! - and, when its sender is told of such refusals, said whether it may be told of this one now: a
//!   few a second each, the rest counted and folded into the next.
//!
//! A kind of message this build doesn't know (`unknown`) is a newer peer's, and a skew between
//! builds can last days, on frames sent many a second: each kind from a sender on a wire is audited
//! and told of once an hour ([`SKEW_EVERY`]), the rest counted, kept for its page and folded into
//! the next its sender is told of. A runner tells the server likewise ([`Told`]), and the server
//! counts its word as the refusals it folds in.
//!
//! The core here opens nothing and reads only the clock it's given. The hub's shell writes the rows
//! it returns, tells the sender when it says so, and says which pages changed.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::hash::Hash;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kiln_seams::Clock;
use serde::{Deserialize, Serialize};

/// The most senders kept at once, for their pages and their audit rows and tells: past it, the
/// least recent is let go.
pub const SENDERS: usize = 4096;
/// How many of a runner's or agent session's newest refusals are kept for its page.
pub const NEWEST: usize = 16;
/// How long a sender, wire and code get one audit row: the rest wait for the row a minute that
/// says how many.
pub const AUDIT_EVERY: Duration = Duration::from_secs(60);
/// How many refusals a second a sender is told of: the rest are counted and folded into the next.
pub const TELL_PER_SECOND: u32 = 16;
/// How long a kind of message a sender's newer build sends (`unknown`) is audited and told of once,
/// for each sender, wire and kind: the rest say nothing new.
pub const SKEW_EVERY: Duration = Duration::from_secs(60 * 60);
/// How long a runner holds what it folded before telling the server of it anyway ([`Told::flush`]):
/// no refusal goes untold for longer, though no other word comes.
pub const FOLD_FLUSH: Duration = Duration::from_secs(60);
/// The most kinds of the server's a runner keeps having told it of on one connection ([`Told`]):
/// past it, a new one is folded into the next word.
pub const TOLD_KINDS: usize = 1024;
/// The most of a refusal's `why`, and of its audit row's detail, in bytes.
pub const WHY_MOST: usize = 512;
/// The most of a refusal's `what`, in bytes: it names a kind of message, never what one said.
pub const WHAT_MOST: usize = 128;
/// The audit log's action for a refusal.
pub const ACTION: &str = "security.wire_refused";

/// A wire, and which way along it: what reads it and what sent it. It travels in frames
/// (`AgentMsg::Unread`, `ServerMsg::Unread`): new ones are appended last, as postcard encodes
/// variants by index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum Wire {
    /// A runner's frames, as the server reads them.
    Runner,
    /// The server's frames, as a runner reads them.
    Server,
    /// The items of a runner's channel streams (`wire::channel`), as the server reads them.
    RunnerItem,
    /// The items of the server's channel streams, as a runner reads them.
    ServerItem,
    /// The JSON a runner's frame carries (a session's updates, an inventory), as the server reads it.
    RunnerPayload,
    /// The JSON the server's frame carries (a session's commands), as a runner reads it.
    ServerPayload,
    /// What runners send the server's relay for one another (reach's links).
    Relay,
    /// What a runner reads inside a reach link, from the runner at its other end.
    ReachLink,
    /// The session socket: what a page sends up.
    SessionUp,
    /// The session socket: what a page couldn't read of what came down.
    SessionDown,
    /// A terminal's socket (a session's, a runner's, a CI leg's): what a page sends up.
    TerminalUp,
    /// A terminal's socket: what a page couldn't read of what came down.
    TerminalDown,
    /// A stream's socket: what a page sends up.
    StreamUp,
    /// A stream's socket: what a page couldn't read of what came down.
    StreamDown,
    /// The events socket: what a page sends up.
    EventsUp,
    /// The events socket: what a page couldn't read of what came down.
    EventsDown,
    /// A log's socket: what a page sends up.
    LogUp,
    /// A log's socket: what a page couldn't read of what came down.
    LogDown,
    /// A tunnel's socket: what a page sends up.
    TunnelUp,
    /// A tunnel's socket: what a page couldn't read of what came down.
    TunnelDown,
    /// kiln's MCP endpoint, over HTTP.
    McpHttp,
    /// kiln's MCP calls a runner makes through reach.
    McpReach,
}

impl Wire {
    /// Every wire, matched in [`Wire::name`] without a catch-all arm.
    pub const ALL: [Wire; 22] = [
        Wire::Runner,
        Wire::Server,
        Wire::RunnerItem,
        Wire::ServerItem,
        Wire::RunnerPayload,
        Wire::ServerPayload,
        Wire::Relay,
        Wire::ReachLink,
        Wire::SessionUp,
        Wire::SessionDown,
        Wire::TerminalUp,
        Wire::TerminalDown,
        Wire::StreamUp,
        Wire::StreamDown,
        Wire::EventsUp,
        Wire::EventsDown,
        Wire::LogUp,
        Wire::LogDown,
        Wire::TunnelUp,
        Wire::TunnelDown,
        Wire::McpHttp,
        Wire::McpReach,
    ];

    /// Its name: the metric's label, and as it's written in JSON and in docs/wire.md.
    pub fn name(self) -> &'static str {
        match self {
            Wire::Runner => "runner",
            Wire::Server => "server",
            Wire::RunnerItem => "runner_item",
            Wire::ServerItem => "server_item",
            Wire::RunnerPayload => "runner_payload",
            Wire::ServerPayload => "server_payload",
            Wire::Relay => "relay",
            Wire::ReachLink => "reach_link",
            Wire::SessionUp => "session_up",
            Wire::SessionDown => "session_down",
            Wire::TerminalUp => "terminal_up",
            Wire::TerminalDown => "terminal_down",
            Wire::StreamUp => "stream_up",
            Wire::StreamDown => "stream_down",
            Wire::EventsUp => "events_up",
            Wire::EventsDown => "events_down",
            Wire::LogUp => "log_up",
            Wire::LogDown => "log_down",
            Wire::TunnelUp => "tunnel_up",
            Wire::TunnelDown => "tunnel_down",
            Wire::McpHttp => "mcp_http",
            Wire::McpReach => "mcp_reach",
        }
    }
}

/// Why a message couldn't be read. It travels in frames, as [`Wire`] does: appended to, never
/// reordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "UnreadCode"))]
#[serde(rename_all = "snake_case")]
pub enum Code {
    /// A kind of message this build doesn't know: a newer peer's.
    Unknown,
    /// It couldn't be read: cut short, bytes left over, too large, out of order, or not this
    /// sender's link.
    Malformed,
}

impl Code {
    pub const ALL: [Code; 2] = [Code::Unknown, Code::Malformed];

    pub fn name(self) -> &'static str {
        match self {
            Code::Unknown => "unknown",
            Code::Malformed => "malformed",
        }
    }

    /// As the channel and the UI's nacks say it: the kinds they already have, so an older peer
    /// reads it as before.
    pub fn refusal(self) -> super::channel::Refusal {
        match self {
            Code::Unknown => super::channel::Refusal::Unsupported,
            Code::Malformed => super::channel::Refusal::Malformed,
        }
    }
}

/// Where a message that couldn't be read came from.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "UnreadSender"))]
#[serde(rename_all = "snake_case")]
pub enum Sender {
    /// A runner, by its name, and which of its processes (its instance).
    Runner { name: String, instance: String },
    /// A runner not yet known by its Hello: its certificate's fingerprint and its address.
    Peer { cert: String, addr: String },
    /// A person, by their user name.
    Person { user: String },
    /// An agent session, by its id.
    Session { id: String },
    /// Only its address is known.
    Address { addr: String },
}

impl Sender {
    /// Who it is in the audit log.
    pub fn principal(&self) -> String {
        match self {
            Sender::Runner { name, .. } => format!("runner:{name}"),
            Sender::Peer { cert, .. } => format!("peer:{cert}"),
            Sender::Person { user } => user.clone(),
            Sender::Session { id } => format!("agent:{id}"),
            Sender::Address { addr } => format!("address:{addr}"),
        }
    }

    /// Where it connected from, when that's what's known of it.
    pub fn address(&self) -> Option<&str> {
        match self {
            Sender::Peer { addr, .. } | Sender::Address { addr } => Some(addr.as_str()),
            Sender::Runner { .. } | Sender::Person { .. } | Sender::Session { .. } => None,
        }
    }

    /// The page its refusals show on: a runner's or an agent session's.
    pub fn page(&self) -> Option<Page> {
        match self {
            Sender::Runner { name, .. } => Some(Page::Runner(name.clone())),
            Sender::Session { id } => Some(Page::Session(id.clone())),
            Sender::Peer { .. } | Sender::Person { .. } | Sender::Address { .. } => None,
        }
    }
}

/// A page refusals are kept for.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Page {
    /// A runner's, by its name.
    Runner(String),
    /// An agent session's, by its id.
    Session(String),
}

/// A message a wire couldn't read: never what it said, only what kind it was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Unread {
    pub wire: Wire,
    pub from: Sender,
    /// What it was: a variant's index, a tag, a method or a tool.
    pub what: String,
    pub code: Code,
    pub why: String,
}

impl Unread {
    /// Its `what` and `why` cut to their most, each at a character's end.
    pub(crate) fn cut(mut self) -> Unread {
        cut(&mut self.what, WHAT_MOST);
        cut(&mut self.why, WHY_MOST);
        self
    }
}

/// `s` cut to at most `most` bytes, at a character's end.
fn cut(s: &mut String, most: usize) {
    if s.len() > most {
        let mut end = most;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
}

/// How many refusals of one wire and code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "RefusedCount"))]
pub struct Count {
    pub wire: Wire,
    pub code: Code,
    pub n: u64,
}

/// A refusal kept for a page, and when it was noted (milliseconds since the Unix epoch).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "RefusedMessage"))]
pub struct Refused {
    pub at: i64,
    pub unread: Unread,
}

/// What's kept of a runner's or an agent session's refusals, for its page.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "RefusedMessages"))]
pub struct Kept {
    /// How many, by wire and code, in the order each first came.
    pub counts: Vec<Count>,
    /// The newest, the newest last.
    #[cfg_attr(test, ts(as = "Vec<Refused>"))]
    pub newest: VecDeque<Refused>,
}

impl Kept {
    fn add(&mut self, at: i64, u: &Unread, n: u64) {
        match self.counts.iter_mut().find(|c| c.wire == u.wire && c.code == u.code) {
            Some(c) => c.n = c.n.saturating_add(n),
            None => self.counts.push(Count { wire: u.wire, code: u.code, n }),
        }
        if self.newest.len() >= NEWEST {
            self.newest.pop_front();
        }
        self.newest.push_back(Refused { at, unread: u.clone() });
    }

    /// How many in all.
    pub fn total(&self) -> u64 {
        self.counts.iter().map(|c| c.n).fold(0, u64::saturating_add)
    }
}

/// A row for the audit log: `ACTION`, by `principal`, on the wire, with the code as its result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub principal: String,
    pub target: &'static str,
    pub result: &'static str,
    /// What and why: at most [`WHY_MOST`] bytes.
    pub detail: String,
    pub address: Option<String>,
}

/// What noting a refusal asks of the shell.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Noted {
    /// Tell its sender now, and how many before it it wasn't told of; none: not now (it's counted,
    /// and folded into the next it's told of). Always none from [`Refusals::note`].
    pub tell: Option<u64>,
    /// Rows for the audit log, now.
    pub rows: Vec<Row>,
    /// The pages whose refusals changed.
    pub changed: Vec<Page>,
}

/// One sender's audit rows for one wire and code, over the minute since the last row.
struct Window {
    /// When the last row went: none, no row within the minute.
    since: Option<Instant>,
    /// How many came since then, without a row of their own.
    held: u64,
    /// The newest of them.
    newest: Option<Unread>,
}

/// One sender's tells over the current second.
struct Tells {
    second: Instant,
    told: u32,
    untold: u64,
}

/// Every wire's refusals, noted.
pub struct Refusals {
    clock: Arc<dyn Clock>,
    kept: Recent<Page, Kept>,
    audits: Recent<(Sender, Wire, Code), Window>,
    tells: Recent<Sender, Tells>,
    /// When each unknown kind from a sender on a wire was last audited and told of.
    skews: Recent<(Sender, Wire, String), Option<Instant>>,
}

impl Refusals {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Refusals { clock, kept: Recent::new(SENDERS), audits: Recent::new(SENDERS), tells: Recent::new(SENDERS), skews: Recent::new(SENDERS) }
    }

    /// Note a refusal: counted, kept for its sender's page and the page `on` names (an agent
    /// session's, for a runner's update of it), and audited. What it returns says which rows to
    /// write and which pages changed. Its sender isn't told of it here (a NACK answered it, say, or
    /// it's a runner's word of a frame of the server's): [`Refusals::note_told`] is for one it is.
    pub fn note(&mut self, u: Unread, on: Option<Page>) -> Noted {
        self.noted(u, on, false, 1)
    }

    /// As [`Refusals::note`], for a refusal its sender is told of (`Unread`): it also says whether
    /// it may be told now, and how many before it weren't. Only these count against a sender's
    /// [`TELL_PER_SECOND`] and are folded into the next, so what a sender is told counts exactly
    /// the refusals it's told of.
    pub fn note_told(&mut self, u: Unread, on: Option<Page>) -> Noted {
        self.noted(u, on, true, 1)
    }

    /// A runner's word of a frame of the server's it couldn't read (`AgentMsg::Unread`), with the
    /// `folded` before it it didn't tell of: noted as that many, audited once.
    pub fn note_folded(&mut self, u: Unread, on: Option<Page>, folded: u64) -> Noted {
        self.noted(u, on, false, folded.saturating_add(1))
    }

    fn noted(&mut self, u: Unread, on: Option<Page>, told: bool, n: u64) -> Noted {
        let u = u.cut();
        let (now, at) = (self.clock.now(), self.clock.unix_ms());
        crate::metrics::wire().refused.add(&[u.wire.name(), u.code.name()], n);
        let mut changed: Vec<Page> = Vec::new();
        for page in u.from.page().into_iter().chain(on) {
            if !changed.contains(&page) {
                self.kept.entry(page.clone(), Kept::default).0.add(at, &u, n);
                changed.push(page);
            }
        }
        // A kind a newer build sends, said within the hour: counted and kept, and folded into the
        // next its sender is told of, but no row or word of its own.
        let again = u.code == Code::Unknown && self.said_within(now, &u);
        let rows = if again { Vec::new() } else { self.audit(now, &u) };
        let tell = match (told, again) {
            (false, _) => None,
            (true, true) => {
                self.fold(now, &u.from);
                None
            }
            (true, false) => self.tell(now, &u.from),
        };
        Noted { tell, rows, changed }
    }

    /// Whether `u`'s kind, from its sender on its wire, was audited and told of within
    /// [`SKEW_EVERY`]; if not, it is now.
    fn said_within(&mut self, now: Instant, u: &Unread) -> bool {
        let (at, _) = self.skews.entry((u.from.clone(), u.wire, u.what.clone()), || None);
        match at {
            Some(t) if now.saturating_duration_since(*t) < SKEW_EVERY => true,
            _ => {
                *at = Some(now);
                false
            }
        }
    }

    /// The rows a minute that are due: for each sender, wire and code whose minute is over, how
    /// many came in it after its row.
    pub fn tick(&mut self) -> Vec<Row> {
        let now = self.clock.now();
        let mut rows = Vec::new();
        for ((from, wire, code), w) in self.audits.iter_mut() {
            let Some(since) = w.since else { continue };
            if now.saturating_duration_since(since) < AUDIT_EVERY {
                continue;
            }
            match w.newest.take() {
                Some(newest) if w.held > 0 => {
                    rows.push(summary(from, *wire, *code, w.held, &newest));
                    *w = Window { since: Some(now), held: 0, newest: None };
                }
                _ => w.since = None,
            }
        }
        // A minute over with nothing in it: the next goes at once.
        self.audits.retain(|_, w| w.since.is_some());
        rows
    }

    /// What's kept for `page`.
    pub fn kept(&self, page: &Page) -> Option<&Kept> {
        self.kept.get(page)
    }

    fn audit(&mut self, now: Instant, u: &Unread) -> Vec<Row> {
        let mut rows = Vec::new();
        let (w, gone) = self.audits.entry((u.from.clone(), u.wire, u.code), || Window { since: None, held: 0, newest: None });
        match w.since {
            Some(since) if now.saturating_duration_since(since) < AUDIT_EVERY => {
                w.held = w.held.saturating_add(1);
                w.newest = Some(u.clone());
            }
            // Its minute is over, with some held: they get their row, and this one waits for the
            // next minute's.
            Some(_) if w.held > 0 => {
                if let Some(newest) = w.newest.take() {
                    rows.push(summary(&u.from, u.wire, u.code, w.held, &newest));
                }
                *w = Window { since: Some(now), held: 1, newest: Some(u.clone()) };
            }
            _ => {
                rows.push(row(&u.from, u.wire, u.code, format!("{}: {}", u.what, u.why)));
                *w = Window { since: Some(now), held: 0, newest: None };
            }
        }
        // A sender let go to make room says how many it held.
        if let Some(((from, wire, code), w)) = gone
            && let Some(newest) = w.newest
            && w.held > 0
        {
            rows.push(summary(&from, wire, code, w.held, &newest));
        }
        rows
    }

    /// One its sender isn't told of now, folded into the next it is.
    fn fold(&mut self, now: Instant, from: &Sender) {
        let (t, _) = self.tells.entry(from.clone(), || Tells { second: now, told: 0, untold: 0 });
        t.untold = t.untold.saturating_add(1);
    }

    fn tell(&mut self, now: Instant, from: &Sender) -> Option<u64> {
        let (t, _) = self.tells.entry(from.clone(), || Tells { second: now, told: 0, untold: 0 });
        if now.saturating_duration_since(t.second) >= Duration::from_secs(1) {
            t.second = now;
            t.told = 0;
        }
        if t.told < TELL_PER_SECOND {
            t.told += 1;
            Some(std::mem::take(&mut t.untold))
        } else {
            t.untold = t.untold.saturating_add(1);
            None
        }
    }
}

/// What a runner told the server of what it couldn't read of the server's, on one connection: a
/// kind the server's newer build sends (`unknown`) once an hour ([`SKEW_EVERY`]), as it may come on
/// every frame for days, the rest folded into the next word (and told within [`FOLD_FLUSH`] though
/// none comes); what it couldn't read for any other reason, each time.
#[derive(Debug, Default)]
pub struct Told {
    kinds: HashMap<(Wire, String), Instant>,
    untold: u64,
    /// The newest it folded, while any are untold.
    last: Option<Folded>,
}

/// The newest refusal a runner folded, and since when it has held any untold.
#[derive(Debug, Clone)]
pub struct Folded {
    pub wire: Wire,
    pub what: String,
    pub code: Code,
    pub why: String,
    since: Instant,
}

impl Told {
    /// Whether to tell the server of this one now, and how many before it it didn't (`folded`).
    pub fn tell(&mut self, now: Instant, wire: Wire, code: Code, what: &str, why: &str) -> Option<u64> {
        if code == Code::Unknown {
            let key = (wire, what.to_owned());
            let said = self.kinds.get(&key).is_some_and(|at| now.saturating_duration_since(*at) < SKEW_EVERY);
            if said || (!self.kinds.contains_key(&key) && self.kinds.len() >= TOLD_KINDS) {
                self.untold = self.untold.saturating_add(1);
                let since = self.last.as_ref().map_or(now, |l| l.since);
                self.last = Some(Folded { wire, what: what.to_owned(), code, why: why.to_owned(), since });
                return None;
            }
            self.kinds.insert(key, now);
        }
        self.last = None;
        Some(std::mem::take(&mut self.untold))
    }

    /// What it folded that no word has carried for [`FOLD_FLUSH`]: the newest of them, to tell the
    /// server of now, and how many before it (`folded`).
    pub fn flush(&mut self, now: Instant) -> Option<(Folded, u64)> {
        if now.saturating_duration_since(self.last.as_ref()?.since) < FOLD_FLUSH {
            return None;
        }
        let last = self.last.take()?;
        Some((last, std::mem::take(&mut self.untold).saturating_sub(1)))
    }
}

fn row(from: &Sender, wire: Wire, code: Code, mut detail: String) -> Row {
    cut(&mut detail, WHY_MOST);
    Row { principal: from.principal(), target: wire.name(), result: code.name(), detail, address: from.address().map(str::to_owned) }
}

/// The row for `n` refusals that came in a minute after the one that had its row.
fn summary(from: &Sender, wire: Wire, code: Code, n: u64, newest: &Unread) -> Row {
    row(from, wire, code, format!("{n} more in the minute; the newest, {}: {}", newest.what, newest.why))
}

/// A map of at most `most` entries: the least recently touched is let go to make room.
pub(crate) struct Recent<K, V> {
    most: usize,
    next: u64,
    map: HashMap<K, (u64, V)>,
    /// Each entry's key by when it was last touched.
    order: BTreeMap<u64, K>,
}

impl<K: Clone + Eq + Hash, V> Recent<K, V> {
    pub(crate) fn new(most: usize) -> Self {
        Recent { most, next: 0, map: HashMap::new(), order: BTreeMap::new() }
    }

    /// `k`'s value, made by `new` if it has none, touched; and the entry let go to make room for
    /// it, if one was.
    pub(crate) fn entry(&mut self, k: K, new: impl FnOnce() -> V) -> (&mut V, Option<(K, V)>) {
        self.next += 1;
        let at = self.next;
        let mut gone = None;
        if !self.map.contains_key(&k)
            && self.map.len() >= self.most
            && let Some((_, oldest)) = self.order.pop_first()
        {
            gone = self.map.remove(&oldest).map(|(_, v)| (oldest, v));
        }
        let slot = self.map.entry(k.clone()).or_insert_with(|| (at, new()));
        self.order.remove(&slot.0);
        slot.0 = at;
        self.order.insert(at, k);
        (&mut slot.1, gone)
    }

    fn get(&self, k: &K) -> Option<&V> {
        self.map.get(k).map(|(_, v)| v)
    }

    fn iter_mut(&mut self) -> impl Iterator<Item = (&K, &mut V)> {
        self.map.iter_mut().map(|(k, (_, v))| (k, v))
    }

    fn retain(&mut self, mut keep: impl FnMut(&K, &mut V) -> bool) {
        let order = &mut self.order;
        self.map.retain(|k, (at, v)| {
            let kept = keep(k, v);
            if !kept {
                order.remove(at);
            }
            kept
        });
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.map.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_seams::clock::SimClock;

    fn refusals() -> (SimClock, Refusals) {
        let clock = SimClock::new();
        (clock.clone(), Refusals::new(Arc::new(clock)))
    }

    fn runner(name: &str) -> Sender {
        Sender::Runner { name: name.into(), instance: "main".into() }
    }

    fn unread(from: Sender, wire: Wire, code: Code) -> Unread {
        Unread { wire, from, what: "variant 51".into(), code, why: "AgentMsg has 51 variants".into() }
    }

    #[test]
    fn a_refusal_is_counted_kept_for_its_page_audited_and_its_sender_told() {
        let (_clock, mut r) = refusals();
        let before = crate::metrics::wire().refused.get(&["runner", "unknown"]);
        let noted = r.note_told(unread(runner("fedora"), Wire::Runner, Code::Unknown), None);
        assert!(crate::metrics::wire().refused.get(&["runner", "unknown"]) > before, "counted");
        assert_eq!(noted.tell, Some(0), "told, of none before it");
        assert_eq!(noted.changed, vec![Page::Runner("fedora".into())]);
        assert_eq!(
            noted.rows,
            vec![Row { principal: "runner:fedora".into(), target: "runner", result: "unknown", detail: "variant 51: AgentMsg has 51 variants".into(), address: None }],
            "its audit row, as the plan's example has it"
        );
        let kept = r.kept(&Page::Runner("fedora".into())).cloned().unwrap_or_default();
        assert_eq!(kept.counts, vec![Count { wire: Wire::Runner, code: Code::Unknown, n: 1 }]);
        assert_eq!(kept.newest.len(), 1);
        assert_eq!(kept.total(), 1);
        // A runner's update of an agent session it couldn't read shows on both pages.
        let noted = r.note(unread(runner("fedora"), Wire::RunnerPayload, Code::Malformed), Some(Page::Session("s1".into())));
        assert_eq!(noted.changed, vec![Page::Runner("fedora".into()), Page::Session("s1".into())]);
        assert!(r.kept(&Page::Session("s1".into())).is_some_and(|k| k.total() == 1));
        // A person's has no page of its own: only the one it's for.
        let noted = r.note(unread(Sender::Person { user: "ada".into() }, Wire::SessionUp, Code::Malformed), Some(Page::Session("s1".into())));
        assert_eq!(noted.changed, vec![Page::Session("s1".into())]);
        assert_eq!(noted.rows.first().map(|row| row.principal.as_str()), Some("ada"));
    }

    #[test]
    fn ten_thousand_refusals_in_a_minute_are_two_audit_rows() {
        let (clock, mut r) = refusals();
        let mut rows = Vec::new();
        for _ in 0..10_000 {
            rows.extend(r.note(unread(runner("fedora"), Wire::Runner, Code::Malformed), None).rows);
            clock.advance(Duration::from_millis(5));
            rows.extend(r.tick());
        }
        assert_eq!(rows.len(), 1, "the first at once, the rest held: {rows:?}");
        clock.advance(Duration::from_secs(10));
        rows.extend(r.tick());
        assert_eq!(rows.len(), 2, "and the minute's row: {rows:?}");
        assert_eq!(rows.get(1).map(|row| row.detail.as_str()), Some("9999 more in the minute; the newest, variant 51: AgentMsg has 51 variants"));
        assert_eq!(r.kept(&Page::Runner("fedora".into())).map(Kept::total), Some(10_000), "each still counted for its page");
        // A minute with nothing in it closes the window: the next goes at once.
        clock.advance(AUDIT_EVERY);
        assert!(r.tick().is_empty());
        assert_eq!(r.note(unread(runner("fedora"), Wire::Runner, Code::Malformed), None).rows.len(), 1);
        // Another wire, code or sender has its own minute.
        assert_eq!(r.note(unread(runner("fedora"), Wire::Runner, Code::Unknown), None).rows.len(), 1);
        assert_eq!(r.note(unread(runner("fedora"), Wire::Server, Code::Malformed), None).rows.len(), 1);
        assert_eq!(r.note(unread(runner("mac"), Wire::Runner, Code::Malformed), None).rows.len(), 1);
    }

    #[test]
    fn a_steady_stream_is_one_row_a_minute() {
        let (clock, mut r) = refusals();
        let mut rows = Vec::new();
        // One a second for five minutes.
        for _ in 0..300 {
            rows.extend(r.note(unread(runner("fedora"), Wire::Runner, Code::Malformed), None).rows);
            clock.advance(Duration::from_secs(1));
            rows.extend(r.tick());
        }
        assert_eq!(rows.len(), 6, "the first at once, then one a minute: {rows:?}");
    }

    /// A kind a sender's newer build sends, on every frame for days (a raft's heartbeats to a runner
    /// not yet upgraded): audited and told of once an hour for each sender, wire and kind, each one
    /// still counted, kept for its page, and folded into the next its sender is told of.
    #[test]
    fn an_unknown_kind_is_audited_and_told_of_once_an_hour_each_one_counted() {
        let (clock, mut r) = refusals();
        let observe = || {
            let mut u = unread(runner("fedora"), Wire::RunnerPayload, Code::Unknown);
            u.what = "Observe (teleport)".into();
            u
        };
        let before = crate::metrics::wire().refused.get(&["runner_payload", "unknown"]);
        let first = r.note_told(observe(), None);
        assert_eq!((first.tell, first.rows.len()), (Some(0), 1), "the first, at once");
        for _ in 0..1000 {
            clock.advance(Duration::from_millis(100));
            let n = r.note_told(observe(), None);
            assert_eq!((n.tell, n.rows.len()), (None, 0), "the rest say nothing new");
            assert!(r.tick().is_empty(), "nor in a row a minute");
        }
        assert!(crate::metrics::wire().refused.get(&["runner_payload", "unknown"]) >= before + 1001, "each counted");
        assert_eq!(r.kept(&Page::Runner("fedora".into())).map(Kept::total), Some(1001), "each kept for its page");
        // Another kind, wire or sender is its own, told at once; a malformed one is said as ever.
        let mut other = observe();
        other.what = "Observe (hologram)".into();
        assert_eq!(r.note_told(other, None).tell, Some(1000), "the thousand folded into the next told of");
        let mut wire = observe();
        wire.wire = Wire::Runner;
        assert_eq!(r.note_told(wire, None).tell, Some(0));
        assert_eq!(r.note_told(unread(runner("mac"), Wire::RunnerPayload, Code::Unknown), None).tell, Some(0));
        let mut bad = observe();
        bad.code = Code::Malformed;
        assert!(r.note_told(bad.clone(), None).tell.is_some() && r.note_told(bad, None).tell.is_some());
        // An hour on, it's said again: the skew goes on.
        clock.advance(SKEW_EVERY);
        let again = r.note_told(observe(), None);
        assert_eq!((again.tell, again.rows.len()), (Some(0), 1));
    }

    /// A runner tells the server of each kind the server's newer build sends once an hour on a
    /// connection, the rest folded into its next word, or told within a minute though none comes;
    /// of what it couldn't read for any other reason, each time.
    #[test]
    fn a_runner_tells_of_an_unknown_kind_once_an_hour_the_rest_folded() -> anyhow::Result<()> {
        let t0 = Instant::now();
        let mut told = Told::default();
        let observe = |told: &mut Told, at: Instant, what: &str| told.tell(at, Wire::ServerPayload, Code::Unknown, what, "no kind teleport");
        assert_eq!(observe(&mut told, t0, "Observe (teleport)"), Some(0));
        assert_eq!((observe(&mut told, t0, "Observe (teleport)"), observe(&mut told, t0, "Observe (teleport)")), (None, None));
        assert_eq!(observe(&mut told, t0, "Observe (hologram)"), Some(2), "another kind, told with the two folded");
        assert_eq!(told.tell(t0, Wire::Server, Code::Unknown, "Observe (teleport)", "x"), Some(0), "another wire");
        assert_eq!((told.tell(t0, Wire::Server, Code::Malformed, "variant 9", "cut short"), told.tell(t0, Wire::Server, Code::Malformed, "variant 9", "cut short")), (Some(0), Some(0)), "malformed, each time");
        // Folded with no word after: held a minute, then told, the newest with the rest folded in.
        assert_eq!((observe(&mut told, t0, "Observe (teleport)"), observe(&mut told, t0, "Observe (teleport)")), (None, None));
        assert!(told.flush(t0 + FOLD_FLUSH / 2).is_none(), "not yet");
        let (last, folded) = told.flush(t0 + FOLD_FLUSH).ok_or_else(|| anyhow::anyhow!("not flushed"))?;
        assert_eq!((last.wire, last.what.as_str(), last.code, folded), (Wire::ServerPayload, "Observe (teleport)", Code::Unknown, 1));
        assert!(told.flush(t0 + FOLD_FLUSH * 2).is_none(), "nothing left untold");
        let later = t0 + SKEW_EVERY;
        assert_eq!(observe(&mut told, later, "Observe (teleport)"), Some(0), "an hour on, said again");
        for n in 0..TOLD_KINDS {
            let _ = told.tell(later, Wire::Server, Code::Unknown, &format!("variant {n}"), "x");
        }
        assert_eq!(told.tell(later, Wire::Server, Code::Unknown, "variant past them all", "x"), None, "at most {TOLD_KINDS} kinds kept: the rest folded");
        assert_eq!(Told::default().tell(t0, Wire::ServerPayload, Code::Unknown, "Observe (teleport)", "x"), Some(0), "a new connection tells again");
        Ok(())
    }

    #[test]
    fn a_sender_is_told_of_sixteen_a_second_and_the_rest_are_folded_into_the_next() {
        let (clock, mut r) = refusals();
        let tells: Vec<Option<u64>> = (0..20).map(|_| r.note_told(unread(runner("fedora"), Wire::Runner, Code::Malformed), None).tell).collect();
        assert_eq!(tells.iter().filter(|t| t.is_some()).count(), 16);
        assert!(tells.iter().skip(16).all(Option::is_none), "{tells:?}");
        clock.advance(Duration::from_secs(1));
        assert_eq!(r.note_told(unread(runner("fedora"), Wire::Runner, Code::Malformed), None).tell, Some(4), "the four not told of, folded in");
        assert_eq!(r.note_told(unread(runner("mac"), Wire::Runner, Code::Malformed), None).tell, Some(0), "each sender its own");
    }

    /// What a sender is told counts only what it's told of (typed messages, step 3): a refusal
    /// answered another way (a NACK), or a runner's word of a frame of the server's, takes none of
    /// its sixteen a second and isn't folded into the next it's told of.
    #[test]
    fn what_its_sender_isnt_told_of_is_never_folded_into_what_it_is() {
        let (clock, mut r) = refusals();
        let quiet = |r: &mut Refusals| {
            let item = r.note(unread(runner("fedora"), Wire::RunnerItem, Code::Unknown), None).tell;
            let word = r.note(unread(runner("fedora"), Wire::Server, Code::Malformed), None).tell;
            (item, word)
        };
        for _ in 0..40 {
            assert_eq!(quiet(&mut r), (None, None));
        }
        let tells: Vec<Option<u64>> = (0..18).map(|_| r.note_told(unread(runner("fedora"), Wire::Runner, Code::Malformed), None).tell).collect();
        assert_eq!(tells.iter().filter(|t| **t == Some(0)).count(), 16, "sixteen told, none of the eighty quiet ones folded in: {tells:?}");
        assert_eq!(quiet(&mut r), (None, None));
        clock.advance(Duration::from_secs(1));
        assert_eq!(quiet(&mut r), (None, None));
        assert_eq!(r.note_told(unread(runner("fedora"), Wire::Runner, Code::Malformed), None).tell, Some(2), "the two told-of ones not told, and no quiet one");
        assert_eq!(r.kept(&Page::Runner("fedora".into())).map(Kept::total), Some(84 + 19), "each still counted for its page");
    }

    #[test]
    fn at_most_4096_senders_are_kept_the_least_recent_let_go_first() {
        let (clock, mut r) = refusals();
        let mut rows = 0;
        for n in 0..5000 {
            rows += r.note_told(unread(runner(&format!("r{n}")), Wire::Runner, Code::Unknown), None).rows.len();
            clock.advance(Duration::from_millis(1));
        }
        assert_eq!(rows, 5000, "each one's first goes at once");
        assert_eq!((r.kept.len(), r.audits.len(), r.tells.len(), r.skews.len()), (SENDERS, SENDERS, SENDERS, SENDERS));
        assert!(r.kept(&Page::Runner("r903".into())).is_none(), "the least recent went");
        assert!(r.kept(&Page::Runner("r904".into())).is_some());
        assert!(r.kept(&Page::Runner("r4999".into())).is_some());
        // Touched again, an old one stays and the least recent other goes.
        let _ = r.note(unread(runner("r904"), Wire::Runner, Code::Unknown), None);
        let _ = r.note(unread(runner("new"), Wire::Runner, Code::Unknown), None);
        assert!(r.kept(&Page::Runner("r904".into())).is_some());
        assert!(r.kept(&Page::Runner("r905".into())).is_none());
    }

    #[test]
    fn a_sender_let_go_with_refusals_held_says_how_many() {
        let (_clock, mut r) = refusals();
        for _ in 0..3 {
            let _ = r.note(unread(runner("held"), Wire::Runner, Code::Malformed), None);
        }
        let rows: Vec<Row> = (0..SENDERS).flat_map(|n| r.note(unread(runner(&format!("r{n}")), Wire::Runner, Code::Unknown), None).rows).collect();
        assert!(rows.iter().any(|row| row.principal == "runner:held" && row.detail.starts_with("2 more in the minute")), "{:?}", rows.iter().find(|row| row.principal == "runner:held"));
    }

    #[test]
    fn a_page_keeps_its_newest_sixteen() {
        let (clock, mut r) = refusals();
        for n in 0..20 {
            let mut u = unread(runner("fedora"), Wire::Runner, Code::Unknown);
            u.what = format!("variant {n}");
            let _ = r.note(u, None);
            clock.advance(Duration::from_millis(10));
        }
        let kept = r.kept(&Page::Runner("fedora".into())).cloned().unwrap_or_default();
        assert_eq!(kept.newest.len(), NEWEST);
        assert_eq!(kept.newest.front().map(|k| k.unread.what.as_str()), Some("variant 4"));
        assert_eq!(kept.newest.back().map(|k| k.unread.what.as_str()), Some("variant 19"));
        assert!(kept.newest.iter().zip(kept.newest.iter().skip(1)).all(|(a, b)| a.at < b.at), "oldest first, each with when");
        assert_eq!(kept.total(), 20);
    }

    #[test]
    fn a_why_is_cut_to_512_bytes_and_so_is_its_row() {
        let (_clock, mut r) = refusals();
        let mut u = unread(runner("fedora"), Wire::Runner, Code::Malformed);
        // Three-byte characters, so 512 falls inside one: it's cut at the one before.
        u.why = "€".repeat(300);
        u.what = "x".repeat(1000);
        let noted = r.note(u, None);
        let kept = r.kept(&Page::Runner("fedora".into())).and_then(|k| k.newest.back().cloned()).map(|k| k.unread);
        assert_eq!(kept.as_ref().map(|u| u.why.len()), Some(510));
        assert_eq!(kept.as_ref().map(|u| u.what.len()), Some(WHAT_MOST));
        assert!(noted.rows.iter().all(|row| row.detail.len() <= WHY_MOST), "{:?}", noted.rows.iter().map(|row| row.detail.len()).collect::<Vec<_>>());
    }

    #[test]
    fn an_unread_encodes_in_postcard_and_in_json() -> anyhow::Result<()> {
        let u = unread(Sender::Peer { cert: "ab12".into(), addr: "192.168.1.9:4433".into() }, Wire::Runner, Code::Malformed);
        let back: Unread = postcard::from_bytes(&postcard::to_allocvec(&u)?)?;
        assert_eq!(back, u);
        let json = serde_json::to_value(&u)?;
        assert_eq!(
            json,
            serde_json::json!({"wire": "runner", "from": {"peer": {"cert": "ab12", "addr": "192.168.1.9:4433"}}, "what": "variant 51", "code": "malformed", "why": "AgentMsg has 51 variants"})
        );
        assert_eq!(serde_json::from_value::<Unread>(json)?, u);
        for w in Wire::ALL {
            assert_eq!(serde_json::to_value(w)?, serde_json::json!(w.name()), "{w:?}");
        }
        for c in Code::ALL {
            assert_eq!(serde_json::to_value(c)?, serde_json::json!(c.name()), "{c:?}");
        }
        // As the channel and the UI's nacks already say them.
        assert_eq!(serde_json::to_value(Code::Unknown.refusal())?, serde_json::json!("unsupported"));
        assert_eq!(serde_json::to_value(Code::Malformed.refusal())?, serde_json::json!("malformed"));
        Ok(())
    }

    /// docs/wire.md's "What can't be read" names every code and wire, in their order, and no other,
    /// and says what the constants here do.
    #[test]
    fn the_doc_names_every_code_and_wire_there_is() -> anyhow::Result<()> {
        let doc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/wire.md"))?;
        let section = doc.split("\n## ").find(|s| s.starts_with("What can't be read\n")).ok_or_else(|| anyhow::anyhow!("docs/wire.md has no \"What can't be read\""))?;
        // The first cell of each row of the table whose header begins `| header |`.
        let named = |header: &str| -> Vec<String> {
            let mut rows = section.lines().skip_while(|l| !l.starts_with(&format!("| {header} |"))).skip(1);
            let mut out = Vec::new();
            while let Some(line) = rows.next().filter(|l| l.starts_with('|')) {
                if let Some(name) = line.split('|').nth(1).map(str::trim).and_then(|c| c.strip_prefix('`')).and_then(|c| c.strip_suffix('`')) {
                    out.push(name.to_owned());
                }
            }
            out
        };
        assert_eq!(named("Code"), Code::ALL.map(Code::name).map(str::to_owned).to_vec());
        assert_eq!(named("Wire"), Wire::ALL.map(Wire::name).map(str::to_owned).to_vec());
        let flat = section.split_whitespace().collect::<Vec<_>>().join(" ");
        let senders = format!("{},{:03} senders", SENDERS / 1000, SENDERS % 1000);
        let says = [
            "`kiln_wire_refused_total{wire,code}`".to_owned(),
            format!("`{ACTION}`"),
            format!("the newest {NEWEST}"),
            senders,
            format!("{WHY_MOST} bytes at most"),
            format!("{TELL_PER_SECOND} a second each at most"),
            "one row a minute".to_owned(),
        ];
        for said in says {
            assert!(flat.contains(&said), "docs/wire.md's \"What can't be read\" doesn't say {said}");
        }
        Ok(())
    }
}
