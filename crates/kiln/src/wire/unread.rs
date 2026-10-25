//! What can't be read, on any wire (docs/wire.md, "What can't be read"). One record, [`Unread`], for
//! any message a wire refuses because it can't read it, and one place each is noted,
//! [`Refusals::note`]. A refusal is:
//! - counted, in `kiln_wire_refused_total{wire,code}`, with no label for its sender;
//! - kept for its sender's page: each runner's and agent session's counts by wire and code, and its
//!   newest few;
//! - written to the audit log as `security.wire_refused`, coalesced: the first for a sender, wire and
//!   code in a minute at once, the rest in one row a minute saying how many;
//! - and said whether its sender may be told of it now: a few a second each, the rest counted and
//!   folded into the next.
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
/// The most of a refusal's `why`, and of its audit row's detail, in bytes.
pub const WHY_MOST: usize = 512;
/// The most of a refusal's `what`, in bytes: it names a kind of message, never what one said.
pub const WHAT_MOST: usize = 128;
/// The audit log's action for a refusal.
pub const ACTION: &str = "security.wire_refused";

/// A wire, and which way along it: what reads it and what sent it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
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

/// Why a message couldn't be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
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
    fn cut(mut self) -> Unread {
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
pub struct Count {
    pub wire: Wire,
    pub code: Code,
    pub n: u64,
}

/// A refusal kept for a page, and when it was noted (milliseconds since the Unix epoch).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refused {
    pub at: i64,
    pub unread: Unread,
}

/// What's kept of a runner's or an agent session's refusals, for its page.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Kept {
    /// How many, by wire and code, in the order each first came.
    pub counts: Vec<Count>,
    /// The newest, the newest last.
    pub newest: VecDeque<Refused>,
}

impl Kept {
    fn add(&mut self, at: i64, u: &Unread) {
        match self.counts.iter_mut().find(|c| c.wire == u.wire && c.code == u.code) {
            Some(c) => c.n = c.n.saturating_add(1),
            None => self.counts.push(Count { wire: u.wire, code: u.code, n: 1 }),
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
    /// and folded into the next it's told of).
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
}

impl Refusals {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Refusals { clock, kept: Recent::new(SENDERS), audits: Recent::new(SENDERS), tells: Recent::new(SENDERS) }
    }

    /// Note a refusal: counted, kept for its sender's page and the page `on` names (an agent
    /// session's, for a runner's update of it), and audited. What it returns says which rows to
    /// write, whether to tell its sender, and which pages changed.
    pub fn note(&mut self, u: Unread, on: Option<Page>) -> Noted {
        let u = u.cut();
        let (now, at) = (self.clock.now(), self.clock.unix_ms());
        crate::metrics::wire().refused.inc(&[u.wire.name(), u.code.name()]);
        let mut changed: Vec<Page> = Vec::new();
        for page in u.from.page().into_iter().chain(on) {
            if !changed.contains(&page) {
                self.kept.entry(page.clone(), Kept::default).0.add(at, &u);
                changed.push(page);
            }
        }
        let rows = self.audit(now, &u);
        let tell = self.tell(now, &u.from);
        Noted { tell, rows, changed }
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

fn row(from: &Sender, wire: Wire, code: Code, mut detail: String) -> Row {
    cut(&mut detail, WHY_MOST);
    Row { principal: from.principal(), target: wire.name(), result: code.name(), detail, address: from.address().map(str::to_owned) }
}

/// The row for `n` refusals that came in a minute after the one that had its row.
fn summary(from: &Sender, wire: Wire, code: Code, n: u64, newest: &Unread) -> Row {
    row(from, wire, code, format!("{n} more in the minute; the newest, {}: {}", newest.what, newest.why))
}

/// A map of at most `most` entries: the least recently touched is let go to make room.
struct Recent<K, V> {
    most: usize,
    next: u64,
    map: HashMap<K, (u64, V)>,
    /// Each entry's key by when it was last touched.
    order: BTreeMap<u64, K>,
}

impl<K: Clone + Eq + Hash, V> Recent<K, V> {
    fn new(most: usize) -> Self {
        Recent { most, next: 0, map: HashMap::new(), order: BTreeMap::new() }
    }

    /// `k`'s value, made by `new` if it has none, touched; and the entry let go to make room for
    /// it, if one was.
    fn entry(&mut self, k: K, new: impl FnOnce() -> V) -> (&mut V, Option<(K, V)>) {
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
        let noted = r.note(unread(runner("fedora"), Wire::Runner, Code::Unknown), None);
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
            rows.extend(r.note(unread(runner("fedora"), Wire::Runner, Code::Unknown), None).rows);
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
        assert_eq!(r.note(unread(runner("fedora"), Wire::Runner, Code::Unknown), None).rows.len(), 1);
        // Another wire, code or sender has its own minute.
        assert_eq!(r.note(unread(runner("fedora"), Wire::Runner, Code::Malformed), None).rows.len(), 1);
        assert_eq!(r.note(unread(runner("fedora"), Wire::Server, Code::Unknown), None).rows.len(), 1);
        assert_eq!(r.note(unread(runner("mac"), Wire::Runner, Code::Unknown), None).rows.len(), 1);
    }

    #[test]
    fn a_steady_stream_is_one_row_a_minute() {
        let (clock, mut r) = refusals();
        let mut rows = Vec::new();
        // One a second for five minutes.
        for _ in 0..300 {
            rows.extend(r.note(unread(runner("fedora"), Wire::Runner, Code::Unknown), None).rows);
            clock.advance(Duration::from_secs(1));
            rows.extend(r.tick());
        }
        assert_eq!(rows.len(), 6, "the first at once, then one a minute: {rows:?}");
    }

    #[test]
    fn a_sender_is_told_of_sixteen_a_second_and_the_rest_are_folded_into_the_next() {
        let (clock, mut r) = refusals();
        let tells: Vec<Option<u64>> = (0..20).map(|_| r.note(unread(runner("fedora"), Wire::Runner, Code::Malformed), None).tell).collect();
        assert_eq!(tells.iter().filter(|t| t.is_some()).count(), 16);
        assert!(tells.iter().skip(16).all(Option::is_none), "{tells:?}");
        clock.advance(Duration::from_secs(1));
        assert_eq!(r.note(unread(runner("fedora"), Wire::Runner, Code::Malformed), None).tell, Some(4), "the four not told of, folded in");
        assert_eq!(r.note(unread(runner("mac"), Wire::Runner, Code::Malformed), None).tell, Some(0), "each sender its own");
    }

    #[test]
    fn at_most_4096_senders_are_kept_the_least_recent_let_go_first() {
        let (clock, mut r) = refusals();
        let mut rows = 0;
        for n in 0..5000 {
            rows += r.note(unread(runner(&format!("r{n}")), Wire::Runner, Code::Unknown), None).rows.len();
            clock.advance(Duration::from_millis(1));
        }
        assert_eq!(rows, 5000, "each one's first goes at once");
        assert_eq!((r.kept.len(), r.audits.len(), r.tells.len()), (SENDERS, SENDERS, SENDERS));
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
            let _ = r.note(unread(runner("held"), Wire::Runner, Code::Unknown), None);
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
