//! A deterministic simulation of sending messages to agents (CP-T3.2, AC-T3.2.2), kiln-sim's way
//! (CP-T6.1): one seed decides everything that happens, and a failure replays from its seed.
//!
//! Originators make each message's id once and send it again under that id until it's taken: a page
//! sends twice at once (it reconnected), and every originator sends again when no answer comes, or
//! when one refuses it for an epoch the agent left or a machine it isn't on. A server stamps each
//! message with the epoch it last heard its session is at, waits for the answer (and stops waiting),
//! and sends once more for the epoch a refusal names. Runners hold the sessions, each with the real
//! [`Inbox`] over the records simulator, which a runner's restart keeps (they're on its disk).
//! Between them all, the network nemesis: frames dropped, duplicated, delayed (so out of order), and
//! links cut for a while. Runners restart, the server restarts (forgetting who waits), and sessions
//! move between runners as kiln's code moves them: a move takes a while, the session refusing
//! messages meanwhile (`moved`, before its inbox takes them), and at its end the runner it left
//! fences its inbox and the newest ids it took go with it. The server hears of a move late, and
//! sometimes of where before its epoch or of its epoch before where. A runner keeps its record of
//! its sessions a while after one leaves it: restarted before then, it holds a copy left behind,
//! whose fenced inbox must refuse what reaches it.
//!
//! It runs kiln's own code where a send is decided: a runner takes a message with the host's
//! `take_message` (the moving session's refusal, its inbox, and without inboxes unchecked), a
//! session arrives with the host's `arrive` and leaves with its `carry` and its inbox fenced, and
//! the server sends once more by the registry's `again_for`. Faults it injects into that code: a
//! session that can't take a message as it's handed it (its inbox takes it back, so the retry
//! reaches it), an inbox that can't be opened as its session arrives (it's opened as a message
//! comes), one that can't be read as its session leaves (the move fails), one that can't take in
//! the ids its session brought (they wait, kept in its record), and a move's answer lost after all
//! of it went (the runner it left keeps its inbox fenced and its session taking nothing until it
//! hears, asking again, that it was taken over).
//!
//! What it doesn't model, tested elsewhere: blue/green and its rollback (the host's tests), the
//! hand-over to a machine's newest runner process (`super::super::handing`, `crate::handovers`:
//! the host's tests, and faults' two newer processes given one try), the lanes' timing and the
//! barrier a move reads its ids after (the host's), a session resumed from
//! the server's store, and the inbox's bound of 4,096 ids (a session here is sent tens of messages:
//! the inbox's own tests hold the bound and what a move carries past it). A move's end is one
//! event: the runner it leaves refuses messages from its start, so nothing reaches either inbox
//! between the carry and the arrival. The network, the processes and their timing are the
//! simulation's (kiln-sim, CP-T6.1, takes over whole hosts).
//!
//! The checker reads every harness's prompts. Safety: no message reaches a harness twice, on any
//! runner. Liveness: once the nemesis stops, every message reaches one, once. Its power, shown by
//! bugs planted on purpose ([`Planted`]): without inboxes, with moves that don't fence, that carry no
//! ids, that go on without ids they can't read or that go on where they were when their answer was
//! lost, with a session that takes messages before the ids it brought are in, with a record that
//! doesn't keep those, or with a move that leaves them behind, a message reaches a harness twice.

use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::sync::Arc;

use futures::executor::block_on;
use futures::future::BoxFuture;
use kiln_seams::records::{Changes, Consistency, Page, Records, Refusal as Failed, Rev, Row, Scan, SimRecords, Tx};

use super::super::host::{Taker, arrive, carry as carry_of, take_message};
use super::super::model::{Command, SendMode};
use super::super::registry::again_for;
use super::{Answer, Inbox, Limits, Refusal};

/// SplitMix64: one seed, one sequence of numbers, whatever the platform or the crate versions.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }

    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi.saturating_sub(lo) + 1)
    }

    fn chance(&mut self, per_mille: u64) -> bool {
        self.below(1000) < per_mille
    }

    fn index(&mut self, len: usize) -> usize {
        self.below(len as u64) as usize
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Node {
    Client(usize),
    Server,
    Runner(usize),
}

#[derive(Debug, Clone)]
enum Frame {
    /// A client asks the server to send message `message` to session `session`.
    Send { client: usize, message: String, session: usize },
    /// The server answers a client.
    Answer { message: String, outcome: Outcome },
    /// The server hands a runner a message for one of its sessions, for an epoch.
    Deliver { session: usize, epoch: u64, message: String },
    /// A runner says what became of a message.
    Said { session: usize, message: String, answer: Result<Answer, Refusal> },
    /// A runner announces a session it holds, at its epoch: never lost, only late.
    Announce { session: usize, epoch: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Taken { repeat: bool },
    Refused(&'static str),
    NotThere,
    Unanswered,
}

#[derive(Debug)]
enum Event {
    Arrive { to: Node, from: Node, frame: Frame },
    /// A client looks at what it hasn't had taken.
    Look(usize),
    /// The server stops waiting for an answer to a message's `attempt`.
    GiveUp { message: String, attempt: u64 },
    Down(usize),
    Up(usize),
    ServerRestart,
    Move,
    /// A move under way ends: the session goes on on runner `to` (if both are up, and runner
    /// `from` is still the process `life` that began it).
    MoveDone { session: usize, from: usize, to: usize, life: u64 },
    /// A runner's record of its sessions is written, by its process `life`.
    Save { runner: usize, life: u64 },
    /// Runner `runner` (still its process `life`) hears, asking again, that the runner a session
    /// went to took it over, its answer lost the first time: it lets the session go.
    Settled { session: usize, runner: usize, life: u64 },
    Cut,
}

/// A bug planted on purpose, to show the checker finds what it's for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Planted {
    None,
    /// No inboxes: every message a runner gets goes on to its harness.
    NoInboxes,
    /// A move doesn't fence the inbox of the runner it left.
    NoFence,
    /// A move carries none of the ids its session took.
    NoCarry,
    /// A move whose ids can't be read goes on without them (where kiln's fails).
    CarryIgnored,
    /// A session whose inbox can't take in the ids it brought takes messages anyway (where kiln's
    /// refuses them until they're in).
    ImportIgnored,
    /// A runner's record of its sessions keeps none of the ids they brought that aren't taken in
    /// yet: a restart forgets them (where kiln's record keeps them till they're in).
    WaitingUnrecorded,
    /// A session moving on carries only the ids its inbox took in, none of those it brought that
    /// aren't taken in yet (where kiln's carries them too).
    WaitingNotCarried,
    /// A move whose answer was lost (it went all the way: it runs where it went) lifts the fence of
    /// the inbox it left and goes on there too (where kiln's keeps the fence, and waits to hear).
    UnknownUnfenced,
}

/// A runner's records as its inbox uses them: the simulator's (kept across its restarts), failing
/// its next commits, or its scans, when the run says: a write that failed, a read.
struct Faults {
    inner: SimRecords,
    fail_commits: std::sync::atomic::AtomicU64,
    fail_scans: std::sync::atomic::AtomicBool,
}

impl Faults {
    fn new() -> Arc<Self> {
        Arc::new(Self { inner: SimRecords::new(), fail_commits: std::sync::atomic::AtomicU64::new(0), fail_scans: std::sync::atomic::AtomicBool::new(false) })
    }

    fn healed(&self) {
        self.fail_commits.store(0, std::sync::atomic::Ordering::SeqCst);
        self.fail_scans.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Records for Faults {
    fn get<'a>(&'a self, family: &'a str, key: &'a [u8], at: Consistency) -> BoxFuture<'a, Result<Option<Row>, Failed>> {
        self.inner.get(family, key, at)
    }
    fn scan<'a>(&'a self, family: &'a str, scan: Scan) -> BoxFuture<'a, Result<Page, Failed>> {
        if self.fail_scans.load(std::sync::atomic::Ordering::SeqCst) {
            return Box::pin(async { Err(Failed::Unavailable("a read failed".into())) });
        }
        self.inner.scan(family, scan)
    }
    fn commit<'a>(&'a self, tx: Tx) -> BoxFuture<'a, Result<Rev, Failed>> {
        // One at a time (the simulator runs one thing at once): a failure to come is one fewer.
        let n = self.fail_commits.load(std::sync::atomic::Ordering::SeqCst);
        let failing = n > 0 && self.fail_commits.compare_exchange(n, n - 1, std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst).is_ok();
        if failing {
            return Box::pin(async { Err(Failed::Unavailable("a write failed".into())) });
        }
        self.inner.commit(tx)
    }
    fn changes<'a>(&'a self, family: &'a str, after: Rev, limit: usize) -> BoxFuture<'a, Result<Changes, Failed>> {
        self.inner.changes(family, after, limit)
    }
    fn wait<'a>(&'a self, family: &'a str, after: Rev) -> BoxFuture<'a, Result<Rev, Failed>> {
        self.inner.wait(family, after)
    }
    fn head(&self) -> BoxFuture<'_, Result<Rev, Failed>> {
        self.inner.head()
    }
}

/// How a page and an agent send again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Sends each message twice at once (it reconnected), then again when it isn't taken.
    Page,
    /// Sends again when it isn't taken.
    Agent,
}

struct Message {
    id: String,
    session: usize,
    taken: bool,
    sends: u64,
    last: u64,
}

struct Client {
    kind: Kind,
    messages: Vec<Message>,
}

struct Wait {
    client: usize,
    session: usize,
    attempt: u64,
    again: bool,
}

struct Runner {
    up: bool,
    records: Arc<Faults>,
    inbox: Inbox,
    /// Sessions it holds, at their epochs.
    sessions: BTreeMap<usize, u64>,
    /// Its record of them, on its disk, what it holds again after a restart: a session that came
    /// is in it at once, one that left only once it's written again ([`Event::Save`]). Each at its
    /// epoch, with the ids it brought that its inbox hadn't taken in yet as it was written.
    saved: BTreeMap<usize, (u64, Vec<(String, String)>)>,
    /// Sessions on their way elsewhere: they take no messages.
    moving: BTreeSet<usize>,
    /// Its process: one more at each restart.
    life: u64,
}

/// A session as a runner holds it, for the host's take of a message ([`take_message`]): what it's
/// handed goes to its harness.
struct Held {
    epoch: u64,
    moving: bool,
    /// It can't take what it's handed now (it ended a moment ago, or began to move): injected.
    fails: bool,
    /// It was handed a message.
    tried: std::cell::Cell<bool>,
    handed: RefCell<Vec<String>>,
}

impl Taker for Held {
    fn is_moving(&self) -> bool {
        self.moving
    }

    fn epoch(&self) -> u64 {
        self.epoch
    }

    fn hand(&self, c: Command) -> bool {
        self.tried.set(true);
        if self.fails {
            return false;
        }
        if let Command::Send { id, .. } = c {
            self.handed.borrow_mut().push(id);
        }
        true
    }
}

/// What a run saw, counted (and, across seeds, required to be more than nothing: a fault never made
/// tests nothing).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counts {
    pub messages: u64,
    pub sends: u64,
    pub dropped: u64,
    pub duplicated: u64,
    pub cut: u64,
    pub repeats: u64,
    pub stale: u64,
    pub moves: u64,
    pub restarts: u64,
    pub server_restarts: u64,
    pub unanswered: u64,
    /// Messages refused `moved`: by a session on its way elsewhere, or a fenced copy left behind.
    pub moved: u64,
    /// Messages refused for no session on the runner they reached.
    pub no_session: u64,
    /// Copies of a session that left a runner, held again after it restarted.
    pub left_behind: u64,
    /// Moves that didn't happen (a runner went down while one was under way).
    pub moves_failed: u64,
    /// Messages a session couldn't take as it was handed them (injected): its inbox takes them
    /// back, so the retry reaches it.
    pub hand_failed: u64,
    /// Sessions whose inbox couldn't be opened as they arrived (injected; their ids were still
    /// taken): it's opened as a message comes.
    pub open_failed: u64,
    /// Moves whose ids couldn't be read as they left (each move failed), and arrivals that
    /// couldn't take in the ids they brought (each refusing messages till they were in).
    pub carry_failed: u64,
    pub import_failed: u64,
    /// Moves whose answer was lost after all of it went (the runner it left waits to hear, its
    /// session taking nothing there).
    pub answers_lost: u64,
}

impl Counts {
    fn add(&mut self, o: &Self) {
        self.messages += o.messages;
        self.sends += o.sends;
        self.dropped += o.dropped;
        self.duplicated += o.duplicated;
        self.cut += o.cut;
        self.repeats += o.repeats;
        self.stale += o.stale;
        self.moves += o.moves;
        self.restarts += o.restarts;
        self.server_restarts += o.server_restarts;
        self.unanswered += o.unanswered;
        self.moved += o.moved;
        self.no_session += o.no_session;
        self.left_behind += o.left_behind;
        self.moves_failed += o.moves_failed;
        self.hand_failed += o.hand_failed;
        self.open_failed += o.open_failed;
        self.carry_failed += o.carry_failed;
        self.import_failed += o.import_failed;
        self.answers_lost += o.answers_lost;
    }
}

/// How hard a seed's nemesis is.
#[derive(Debug, Clone, Copy)]
struct Nemesis {
    drop: u64,
    dup: u64,
    delay: u64,
    restarts: u64,
    moves: u64,
    /// The nemesis stops at this tick; after it, every frame arrives (soon) and nothing restarts or
    /// moves.
    until: u64,
}

/// Ticks a client waits for a message to be taken before it sends it again.
const AGAIN: u64 = 80;
/// Ticks the server waits for an answer.
const WAIT: u64 = 50;
/// Most times one message is sent: past it, it's counted as never taken.
const SENDS_MAX: u64 = 400;
/// Events one seed runs, at most.
const EVENTS_MAX: u64 = 200_000;

struct World {
    rng: Rng,
    now: u64,
    seq: u64,
    queue: BinaryHeap<Reverse<(u64, u64)>>,
    events: BTreeMap<u64, Event>,
    nemesis: Nemesis,
    /// A bug planted on purpose (none: kiln's code as it is).
    planted: Planted,
    clients: Vec<Client>,
    view: BTreeMap<usize, (usize, u64)>,
    waiting: BTreeMap<String, Wait>,
    attempts: u64,
    runners: Vec<Runner>,
    /// Where each session is, and at what epoch.
    truth: BTreeMap<usize, (usize, u64)>,
    cuts: BTreeMap<(Node, Node), u64>,
    /// Every prompt a harness got: the message, by runner and session.
    prompts: BTreeMap<String, Vec<(usize, usize)>>,
    counts: Counts,
    /// What happened, folded (two runs of a seed fold alike).
    trace: u64,
}

fn session_id(s: usize) -> String {
    format!("s{s}")
}

fn words(message: &str) -> String {
    format!("the words of {message}")
}

impl World {
    fn new(seed: u64, planted: Planted) -> Self {
        let mut rng = Rng(seed);
        let runners_n = rng.range(2, 3) as usize;
        let sessions_n = rng.range(1, 3) as usize;
        let clients_n = rng.range(2, 4) as usize;
        let nemesis = Nemesis { drop: rng.range(0, 300), dup: rng.range(0, 300), delay: rng.range(2, 40), restarts: rng.range(0, 8), moves: rng.range(0, 8), until: rng.range(500, 3_000) };
        let mut runners: Vec<Runner> = (0..runners_n)
            .map(|_| {
                let records = Faults::new();
                Runner { up: true, inbox: Inbox::new(Arc::clone(&records) as Arc<dyn Records>, Limits::default()), records, sessions: BTreeMap::new(), saved: BTreeMap::new(), moving: BTreeSet::new(), life: 0 }
            })
            .collect();
        let mut truth = BTreeMap::new();
        let mut view = BTreeMap::new();
        for s in 0..sessions_n {
            let r = rng.index(runners_n);
            if let Some(runner) = runners.get_mut(r) {
                runner.sessions.insert(s, 0);
                runner.saved.insert(s, (0, Vec::new()));
                block_on(arrive(&runner.inbox, &session_id(s), 0, &[], false, 0));
            }
            truth.insert(s, (r, 0));
            view.insert(s, (r, 0));
        }
        let mut clients = Vec::new();
        for c in 0..clients_n {
            let kind = if rng.chance(500) { Kind::Page } else { Kind::Agent };
            let n = rng.range(3, 12);
            let messages = (0..n).map(|m| Message { id: format!("m{seed:x}-{c}-{m}"), session: rng.index(sessions_n), taken: false, sends: 0, last: 0 }).collect();
            clients.push(Client { kind, messages });
        }
        let mut w = Self {
            rng,
            now: 0,
            seq: 0,
            queue: BinaryHeap::new(),
            events: BTreeMap::new(),
            nemesis,
            planted,
            clients,
            view,
            waiting: BTreeMap::new(),
            attempts: 0,
            runners,
            truth,
            cuts: BTreeMap::new(),
            prompts: BTreeMap::new(),
            counts: Counts::default(),
            trace: seed,
        };
        w.counts.messages = w.clients.iter().map(|c| c.messages.len() as u64).sum();
        for c in 0..clients_n {
            let at = w.rng.range(0, 50);
            w.at(at, Event::Look(c));
        }
        // The nemesis's own moments: restarts, moves and cut links, while it lasts.
        let until = w.nemesis.until;
        for _ in 0..w.nemesis.restarts {
            let (r, t) = (w.rng.index(runners_n), w.rng.range(0, until));
            w.at(t, Event::Down(r));
        }
        for _ in 0..w.nemesis.moves {
            let t = w.rng.range(0, until);
            w.at(t, Event::Move);
        }
        for _ in 0..w.rng.range(0, 3) {
            let t = w.rng.range(0, until);
            w.at(t, Event::ServerRestart);
        }
        for _ in 0..w.rng.range(0, 6) {
            let t = w.rng.range(0, until);
            w.at(t, Event::Cut);
        }
        w
    }

    fn at(&mut self, t: u64, e: Event) {
        self.seq += 1;
        self.queue.push(Reverse((t, self.seq)));
        self.events.insert(self.seq, e);
    }

    fn faulty(&self) -> bool {
        self.now < self.nemesis.until
    }

    fn fold(&mut self, x: u64) {
        self.trace = (self.trace ^ x).wrapping_mul(0x0100_0000_01B3).rotate_left(5);
    }

    /// A frame on its way: maybe lost, maybe twice, each copy after a while of its own.
    fn net(&mut self, from: Node, to: Node, frame: Frame) {
        let announce = matches!(frame, Frame::Announce { .. });
        if self.faulty() && !announce {
            let key = if from < to { (from, to) } else { (to, from) };
            if self.cuts.get(&key).is_some_and(|until| *until > self.now) {
                self.counts.cut += 1;
                return;
            }
            if self.rng.chance(self.nemesis.drop) {
                self.counts.dropped += 1;
                return;
            }
        }
        let copies = if self.faulty() && !announce && self.rng.chance(self.nemesis.dup) {
            self.counts.duplicated += 1;
            2
        } else {
            1
        };
        for _ in 0..copies {
            let delay = if self.faulty() { self.rng.range(1, self.nemesis.delay) } else { self.rng.range(1, 3) };
            self.at(self.now + delay, Event::Arrive { to, from, frame: frame.clone() });
        }
    }

    /// Runs until every message is taken, or the events run out.
    fn run(&mut self) -> Result<(), String> {
        for _ in 0..EVENTS_MAX {
            let Some(Reverse((t, seq))) = self.queue.pop() else { break };
            let Some(e) = self.events.remove(&seq) else { continue };
            self.now = t;
            self.fold(t);
            self.step(e)?;
            if self.done() {
                break;
            }
        }
        self.check(true)
    }

    fn done(&self) -> bool {
        self.clients.iter().all(|c| c.messages.iter().all(|m| m.taken))
    }

    fn step(&mut self, e: Event) -> Result<(), String> {
        match e {
            Event::Look(c) => self.look(c),
            Event::Arrive { to: Node::Server, from, frame } => self.at_server(from, frame),
            Event::Arrive { to: Node::Runner(r), frame, .. } => self.at_runner(r, frame)?,
            Event::Arrive { to: Node::Client(c), frame, .. } => self.at_client(c, frame),
            Event::GiveUp { message, attempt } => {
                if self.waiting.get(&message).is_some_and(|w| w.attempt == attempt)
                    && let Some(w) = self.waiting.remove(&message)
                {
                    self.counts.unanswered += 1;
                    self.net(Node::Server, Node::Client(w.client), Frame::Answer { message, outcome: Outcome::Unanswered });
                }
            }
            Event::Down(r) => {
                if self.faulty()
                    && let Some(runner) = self.runners.get_mut(r)
                    && runner.up
                {
                    runner.up = false;
                    self.counts.restarts += 1;
                    let back = self.rng.range(5, 120);
                    self.at(self.now + back, Event::Up(r));
                }
            }
            Event::Up(r) => self.up(r),
            Event::MoveDone { session, from, to, life } => self.move_done(session, from, to, life),
            Event::Save { runner, life } => {
                let recorded = self.planted != Planted::WaitingUnrecorded;
                if let Some(x) = self.runners.get_mut(runner)
                    && x.up
                    && x.life == life
                {
                    x.saved = x.sessions.iter().map(|(s, e)| (*s, (*e, if recorded { x.inbox.waiting(&session_id(*s)) } else { Vec::new() }))).collect();
                }
            }
            Event::Settled { session, runner, life } => {
                if let Some(x) = self.runners.get_mut(runner)
                    && x.up
                    && x.life == life
                    && x.moving.remove(&session)
                {
                    x.sessions.remove(&session);
                    let written = self.now + self.rng.range(1, 60);
                    self.at(written, Event::Save { runner, life });
                }
            }
            Event::ServerRestart => {
                // What it waited for is forgotten; whoever asked sends again.
                self.waiting.clear();
                self.counts.server_restarts += 1;
            }
            Event::Move => self.move_begin(),
            Event::Cut => {
                let a = if self.rng.chance(500) { Node::Client(self.rng.index(self.clients.len())) } else { Node::Runner(self.rng.index(self.runners.len())) };
                let until = self.now + self.rng.range(20, 300);
                self.cuts.insert((a.min(Node::Server), a.max(Node::Server)), until);
            }
        }
        self.check(false)
    }

    fn look(&mut self, c: usize) {
        let now = self.now;
        let mut sends = Vec::new();
        let Some(client) = self.clients.get_mut(c) else { return };
        for m in client.messages.iter_mut().filter(|m| !m.taken && m.sends < SENDS_MAX) {
            if m.sends == 0 || now >= m.last + AGAIN {
                let times = if m.sends == 0 && client.kind == Kind::Page { 2 } else { 1 };
                m.sends += times;
                m.last = now;
                for _ in 0..times {
                    sends.push((m.id.clone(), m.session));
                }
            }
        }
        let more = client.messages.iter().any(|m| !m.taken && m.sends < SENDS_MAX);
        for (message, session) in sends {
            self.counts.sends += 1;
            self.net(Node::Client(c), Node::Server, Frame::Send { client: c, message, session });
        }
        if more {
            let next = self.now + AGAIN / 2;
            self.at(next, Event::Look(c));
        }
    }

    fn at_client(&mut self, c: usize, frame: Frame) {
        let Frame::Answer { message, outcome } = frame else { return };
        self.fold(message.len() as u64);
        if let Outcome::Taken { .. } = outcome
            && let Some(m) = self.clients.get_mut(c).and_then(|cl| cl.messages.iter_mut().find(|m| m.id == message))
        {
            m.taken = true;
        }
    }

    fn forward(&mut self, message: &str, session: usize, epoch: Option<u64>) {
        let Some((r, seen)) = self.view.get(&session).copied() else { return };
        self.attempts += 1;
        let attempt = self.attempts;
        if let Some(w) = self.waiting.get_mut(message) {
            w.attempt = attempt;
        }
        self.net(Node::Server, Node::Runner(r), Frame::Deliver { session, epoch: epoch.unwrap_or(seen), message: message.to_owned() });
        self.at(self.now + WAIT, Event::GiveUp { message: message.to_owned(), attempt });
    }

    fn at_server(&mut self, from: Node, frame: Frame) {
        match frame {
            Frame::Send { client, message, session } => {
                self.waiting.insert(message.clone(), Wait { client, session, attempt: 0, again: false });
                self.forward(&message, session, None);
            }
            Frame::Said { session, message, answer } => {
                let Some(w) = self.waiting.get_mut(&message) else { return };
                // Sent for an epoch the session has left: once more, for the one it's at (the
                // registry's rule).
                if let Err(r) = &answer
                    && let Some(current) = again_for(r)
                    && !w.again
                {
                    w.again = true;
                    self.counts.stale += 1;
                    let s = w.session;
                    if let Some(v) = self.view.get_mut(&s)
                        && v.1 < current
                    {
                        v.1 = current;
                    }
                    self.forward(&message, session, Some(current));
                    return;
                }
                let Some(w) = self.waiting.remove(&message) else { return };
                match &answer {
                    Err(Refusal::Moved { .. }) => self.counts.moved += 1,
                    Err(Refusal::NoSession { .. }) => self.counts.no_session += 1,
                    _ => {}
                }
                let outcome = match &answer {
                    Ok(a) => Outcome::Taken { repeat: a.repeat },
                    Err(Refusal::NoSession { .. }) => Outcome::NotThere,
                    Err(r) => Outcome::Refused(r.reason()),
                };
                self.net(Node::Server, Node::Client(w.client), Frame::Answer { message, outcome });
            }
            Frame::Announce { session, epoch } => {
                if let Node::Runner(r) = from
                    && self.view.get(&session).is_none_or(|(_, seen)| epoch >= *seen)
                {
                    self.view.insert(session, (r, epoch));
                }
            }
            Frame::Answer { .. } | Frame::Deliver { .. } => {}
        }
    }

    fn at_runner(&mut self, r: usize, frame: Frame) -> Result<(), String> {
        let (now, faulty) = (self.now, self.faulty());
        let inboxes = self.planted != Planted::NoInboxes;
        let Some(runner) = self.runners.get_mut(r) else { return Ok(()) };
        if !runner.up {
            self.counts.dropped += 1;
            return Ok(());
        }
        let Frame::Deliver { session, epoch, message } = frame else { return Ok(()) };
        // The host's own take of it, down the session's lane: the session as this runner holds it
        // (none: it holds none), its inbox (none: the planted bug, kiln without inboxes).
        let fails = faulty && self.rng.chance(20);
        let Some(runner) = self.runners.get_mut(r) else { return Ok(()) };
        let held = runner.sessions.get(&session).map(|e| Held { epoch: *e, moving: runner.moving.contains(&session), fails, tried: std::cell::Cell::new(false), handed: RefCell::new(Vec::new()) });
        let c = Command::Send { id: message.clone(), text: words(&message), mode: SendMode::Queue, attachments: Vec::new(), overrides: None, reply_to: None, task: None };
        let said = block_on(take_message(inboxes.then_some(&runner.inbox), &session_id(session), held.as_ref(), c, Some(epoch), now as i64));
        let answer = said.map_or_else(|| Err(Refusal::NoSession { session: session_id(session) }), |s| s.answer);
        if fails && held.as_ref().is_some_and(|h| h.tried.get()) {
            self.counts.hand_failed += 1;
        }
        for m in held.map(|h| h.handed.into_inner()).unwrap_or_default() {
            self.prompts.entry(m).or_default().push((r, session));
        }
        if answer.as_ref().is_ok_and(|a| a.repeat) {
            self.counts.repeats += 1;
        }
        self.fold(match &answer {
            Ok(a) => a.seq,
            Err(e) => e.reason().len() as u64,
        });
        self.net(Node::Runner(r), Node::Server, Frame::Said { session, message, answer });
        Ok(())
    }

    /// A runner comes back: its process anew (its inboxes' heads read again, no move under way),
    /// its records as they were on its disk, the sessions its record says it holds opened and
    /// announced: a session that left it since the record was last written comes back as a copy
    /// left behind, at the epoch it left at.
    fn up(&mut self, r: usize) {
        let (now, truth) = (self.now, self.truth.clone());
        let Some(runner) = self.runners.get_mut(r) else { return };
        runner.up = true;
        runner.life += 1;
        runner.moving.clear();
        runner.records.healed();
        runner.inbox = Inbox::new(Arc::clone(&runner.records) as Arc<dyn Records>, Limits::default());
        runner.sessions = runner.saved.iter().map(|(s, (e, _))| (*s, *e)).collect();
        let behind = runner.sessions.keys().filter(|s| truth.get(s).is_none_or(|(at, _)| *at != r)).count();
        self.counts.left_behind += behind as u64;
        let Some(runner) = self.runners.get_mut(r) else { return };
        // Each with the ids its record says it brought and hadn't taken in.
        let held: Vec<(usize, u64, Vec<(String, String)>)> = runner.saved.iter().map(|(s, (e, waiting))| (*s, *e, waiting.clone())).collect();
        for (s, e, waiting) in &held {
            block_on(arrive(&runner.inbox, &session_id(*s), *e, waiting, false, now as i64));
        }
        for (s, e, _) in held {
            self.net(Node::Runner(r), Node::Server, Frame::Announce { session: s, epoch: e });
        }
    }

    /// A session begins to move to another runner that's up: from now on it takes no messages here,
    /// and a while later the move ends ([`World::move_done`]).
    fn move_begin(&mut self) {
        if !self.faulty() || self.truth.is_empty() {
            return;
        }
        let s = self.rng.index(self.truth.len());
        let Some((from, _)) = self.truth.get(&s).copied() else { return };
        let to = self.rng.index(self.runners.len());
        let ups = |w: &World, r: usize| w.runners.get(r).is_some_and(|x| x.up);
        if to == from || !ups(self, from) || !ups(self, to) {
            return;
        }
        let Some(src) = self.runners.get_mut(from) else { return };
        if !src.moving.insert(s) {
            return;
        }
        let life = src.life;
        let ends = self.now + self.rng.range(5, 60);
        self.at(ends, Event::MoveDone { session: s, from, to, life });
    }

    /// A move ends: with both runners up and the process that began it still running, the session
    /// goes on on runner `to`, an epoch up: the runner it left fences its inbox and lets it go, and
    /// the newest ids it took are taken by its inbox there. The server hears of it late (sometimes
    /// of where first, sometimes of its epoch first). Else it stays where it was.
    fn move_done(&mut self, s: usize, from: usize, to: usize, life: u64) {
        let began = self.runners.get(from).is_some_and(|x| x.up && x.life == life && x.moving.contains(&s));
        let there = self.runners.get(to).is_some_and(|x| x.up);
        let Some((at, epoch)) = self.truth.get(&s).copied() else { return };
        if !began || !there || at != from {
            if let Some(src) = self.runners.get_mut(from)
                && src.life == life
            {
                src.moving.remove(&s);
            }
            self.counts.moves_failed += 1;
            return;
        }
        let sid = session_id(s);
        // The runner it goes to holds a copy of it already (left behind, or one whose move away is
        // awaiting its answer): it refuses it, as kiln's says it runs it already.
        let held_there = self.runners.get(to).is_some_and(|x| x.sessions.contains_key(&s));
        let (fence, carry) = (self.planted != Planted::NoFence, self.planted != Planted::NoCarry);
        // Its inbox sometimes can't be read as it leaves.
        let unreadable = self.faulty() && self.rng.chance(40);
        let carried = match self.runners.get(from) {
            Some(src) => {
                if fence {
                    let _ = block_on(src.inbox.fence(&sid, epoch));
                }
                src.records.fail_scans.store(unreadable, std::sync::atomic::Ordering::SeqCst);
                let carried = if carry { block_on(carry_of(&src.inbox, &sid)) } else { Ok(Vec::new()) };
                src.records.fail_scans.store(false, std::sync::atomic::Ordering::SeqCst);
                // Planted: what it brought and hasn't taken in stays behind.
                let waiting = src.inbox.waiting(&sid);
                if self.planted == Planted::WaitingNotCarried { carried.map(|c| c.into_iter().filter(|p| !waiting.contains(p)).collect()) } else { carried }
            }
            None => return,
        };
        if unreadable {
            self.counts.carry_failed += 1;
        }
        // Its ids couldn't be read: it doesn't go without them (kiln's move fails), and takes
        // messages again where it is (planted: it goes without them).
        let carried = if self.planted == Planted::CarryIgnored { Ok(carried.unwrap_or_default()) } else { carried };
        let carried = if held_there { Err(Refusal::Invalid { what: sid.clone(), why: "it runs there already".into() }) } else { carried };
        let Ok(taken) = carried else {
            if let Some(src) = self.runners.get_mut(from) {
                let _ = block_on(src.inbox.take_back(&sid, epoch));
                src.moving.remove(&s);
            }
            self.counts.moves_failed += 1;
            return;
        };
        // Its answer sometimes lost on the way back (all of it went: it runs where it went). The
        // runner it left keeps its inbox fenced and the session moving, taking nothing, until asking
        // again it hears it was taken over (planted: it lifts the fence and goes on here too).
        let answer_lost = self.faulty() && self.rng.chance(300);
        if answer_lost {
            self.counts.answers_lost += 1;
            if self.planted == Planted::UnknownUnfenced {
                if let Some(src) = self.runners.get_mut(from) {
                    let _ = block_on(src.inbox.take_back(&sid, epoch));
                    src.moving.remove(&s);
                }
            } else {
                let heard = self.now + self.rng.range(5, 60);
                self.at(heard, Event::Settled { session: s, runner: from, life });
            }
        } else {
            if let Some(src) = self.runners.get_mut(from) {
                src.sessions.remove(&s);
                src.moving.remove(&s);
            }
            // Its record of its sessions is written a while later: a restart before then holds a
            // copy left behind (and runners do restart soon after a session leaves: an upgrade, a
            // crash).
            let written = self.now + self.rng.range(1, 60);
            self.at(written, Event::Save { runner: from, life });
        }
        if self.faulty() && self.rng.chance(350) {
            let down = self.now + self.rng.range(1, 30);
            self.at(down, Event::Down(from));
        }
        let next = epoch + 1;
        // Its inbox there sometimes can't be opened as it arrives (the ids it brought are still
        // taken): it's opened as a message comes (`take_message`).
        let (open_fails, now) = (self.faulty() && self.rng.chance(60), self.now as i64);
        // And sometimes it can't take in the ids it brought (a write failed): it refuses messages
        // till they're in (planted: it takes them anyway).
        let import_fails = !open_fails && !taken.is_empty() && self.faulty() && self.rng.chance(40);
        let planted = self.planted;
        if let Some(dst) = self.runners.get_mut(to) {
            dst.sessions.insert(s, next);
            // Its record, written as it comes, keeps the ids it brought till they're taken in.
            dst.saved.insert(s, (next, if planted == Planted::WaitingUnrecorded { Vec::new() } else { taken.clone() }));
            if open_fails {
                let _ = block_on(dst.inbox.import_kept(&sid, &taken, now));
            } else if import_fails && planted == Planted::ImportIgnored {
                let _ = block_on(dst.inbox.open(&sid, next));
                dst.records.fail_commits.store(1, std::sync::atomic::Ordering::SeqCst);
                let _ = block_on(dst.inbox.import_kept(&sid, &taken, now));
            } else {
                if import_fails {
                    dst.records.fail_commits.store(1, std::sync::atomic::Ordering::SeqCst);
                }
                block_on(arrive(&dst.inbox, &sid, next, &taken, false, now));
            }
            dst.records.fail_commits.store(0, std::sync::atomic::Ordering::SeqCst);
        }
        if open_fails {
            self.counts.open_failed += 1;
        }
        if import_fails {
            self.counts.import_failed += 1;
        }
        self.truth.insert(s, (to, next));
        self.counts.moves += 1;
        // What the server hears first: where (it sends there for the old epoch, and is told), its
        // epoch (it sends where it was for the new one), or neither till the runner says.
        match self.rng.below(10) {
            0..=3 => {
                if let Some(v) = self.view.get_mut(&s) {
                    v.0 = to;
                }
            }
            4..=5 => {
                if let Some(v) = self.view.get_mut(&s)
                    && v.1 < next
                {
                    v.1 = next;
                }
            }
            _ => {}
        }
        // Said by the runner it went to, never lost, and sometimes heard very late.
        let heard = self.now + if self.rng.chance(500) { self.rng.range(1, 150) } else { self.rng.range(1, 10) };
        self.at(heard, Event::Arrive { to: Node::Server, from: Node::Runner(to), frame: Frame::Announce { session: s, epoch: next } });
    }

    /// Safety at every step: no message reached a harness twice. Liveness at the end: every message
    /// was taken, and reached one harness, once.
    fn check(&self, end: bool) -> Result<(), String> {
        if let Some((m, at)) = self.prompts.iter().find(|(_, at)| at.len() > 1) {
            return Err(format!("{m} reached a harness {} times: {at:?} (by tick {})", at.len(), self.now));
        }
        if end {
            for c in &self.clients {
                for m in &c.messages {
                    let reached = self.prompts.get(&m.id).map_or(0, Vec::len);
                    if !m.taken || reached != 1 {
                        return Err(format!("{} was taken: {}, and reached a harness {reached} times, after {} sends (tick {})", m.id, m.taken, m.sends, self.now));
                    }
                }
            }
        }
        Ok(())
    }
}

/// One seed's run: what it counted and what it folded, or how it failed.
pub fn run(seed: u64, planted: Planted) -> Result<(Counts, u64), String> {
    let mut w = World::new(seed, planted);
    w.run().map_err(|e| format!("seed {seed}: {e}"))?;
    Ok((w.counts, w.trace))
}

/// Seeds to run: `KILN_SIM_SEEDS` when set, else `default`.
fn budget(default: u64) -> u64 {
    std::env::var("KILN_SIM_SEEDS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

/// CP-T3.2, AC-T3.2.2: over the seed budget (1,000 by default; the evidence run sets
/// `KILN_SIM_SEEDS=10000`), no message reaches a harness twice, and every message is taken and
/// reaches one once the nemesis stops. Every fault happened, counted across the seeds.
#[test]
fn no_message_reaches_a_harness_twice_however_it_is_retried_or_duplicated() -> Result<(), String> {
    let seeds = budget(1_000);
    let started = std::time::Instant::now();
    let mut all = Counts::default();
    for seed in 0..seeds {
        let (counts, _) = run(seed, Planted::None)?;
        all.add(&counts);
    }
    eprintln!("sends simulation: {seeds} seeds in {:.1} s, 0 messages reached a harness twice: {all:?}", started.elapsed().as_secs_f64());
    let none = [
        ("sends", all.sends),
        ("dropped frames", all.dropped),
        ("duplicated frames", all.duplicated),
        ("frames over cut links", all.cut),
        ("repeats an inbox answered", all.repeats),
        ("stale epochs sent again", all.stale),
        ("moves", all.moves),
        ("runner restarts", all.restarts),
        ("server restarts", all.server_restarts),
        ("answers that didn't come", all.unanswered),
        ("messages refused while their session moved, or by a fenced copy", all.moved),
        ("messages refused for no session there", all.no_session),
        ("copies left behind", all.left_behind),
        ("messages a session couldn't take as it was handed them", all.hand_failed),
        ("inboxes that couldn't be opened as their session arrived", all.open_failed),
        ("moves whose ids couldn't be read as their session left", all.carry_failed),
        ("arrivals whose inbox couldn't take in the ids they brought", all.import_failed),
        ("moves whose answer was lost", all.answers_lost),
    ];
    match none.iter().find(|(_, n)| *n == 0) {
        Some((what, _)) => Err(format!("no {what} over {seeds} seeds: the simulation didn't test what it says")),
        None => Ok(()),
    }
}

/// CP-T3.2: a seed's run is the same every time: what it counted, and everything it folded.
#[test]
fn a_seed_replays_exactly() -> Result<(), String> {
    for seed in [7, 1_234, 99_999] {
        let (a, b) = (run(seed, Planted::None)?, run(seed, Planted::None)?);
        if a != b {
            return Err(format!("seed {seed} ran two ways: {a:?} and {b:?}"));
        }
    }
    Ok(())
}

/// The first seed below `seeds` at which a message reaches a harness twice with `planted`: the
/// checker finds the bug planted. Another failure is the simulation's own.
fn finds(planted: Planted, seeds: u64) -> Result<u64, String> {
    for seed in 0..seeds {
        if let Err(why) = run(seed, planted) {
            // Safety's failure (a message at a harness more than once, and where), not liveness's.
            if why.contains(" times: [") {
                eprintln!("sends simulation, {planted:?} planted: found at seed {seed}: {why}");
                return Ok(seed);
            }
            return Err(format!("with {planted:?} it failed otherwise: {why}"));
        }
    }
    Err(format!("{seeds} seeds with {planted:?} found no message twice at a harness: the simulation can't see that bug"))
}

/// CP-T3.2: the simulation finds the bug it's for: without inboxes, a retried or duplicated message
/// reaches a harness twice within a few seeds.
#[test]
fn without_inboxes_a_message_reaches_a_harness_twice() -> Result<(), String> {
    finds(Planted::NoInboxes, 200).map(|_| ())
}

/// CP-T3.2: a move that doesn't fence the inbox it leaves lets a copy left behind take a message
/// that also reaches the session where it went: the checker finds it.
#[test]
fn moves_that_do_not_fence_let_a_message_reach_a_harness_twice() -> Result<(), String> {
    finds(Planted::NoFence, 10_000).map(|_| ())
}

/// CP-T3.2: a move that carries none of the ids its session took lets a retry of one be taken again
/// where it went: the checker finds it.
#[test]
fn moves_that_carry_no_ids_let_a_message_reach_a_harness_twice() -> Result<(), String> {
    finds(Planted::NoCarry, 2_000).map(|_| ())
}

/// CP-T3.2 (the fifth adversarial check's): a move whose ids can't be read, gone on without them,
/// lets a retry be taken again where it went: the checker finds it (kiln's move fails instead).
#[test]
fn moves_that_go_on_without_their_unreadable_ids_let_a_message_reach_a_harness_twice() -> Result<(), String> {
    finds(Planted::CarryIgnored, 20_000).map(|_| ())
}

/// CP-T3.2 (the fifth adversarial check's): a session that takes messages before the ids it brought
/// are in lets a retry be taken again: the checker finds it (kiln's refuses them till they're in).
#[test]
fn a_session_that_takes_messages_before_its_ids_are_in_lets_a_message_reach_a_harness_twice() -> Result<(), String> {
    finds(Planted::ImportIgnored, 20_000).map(|_| ())
}

/// CP-T3.2: a runner restarted before a session that came to it took in the ids it brought, its
/// record without them, lets a retry be taken again: the checker finds it (kiln's record keeps
/// them till they're in).
#[test]
fn a_restart_that_forgets_the_ids_a_session_brought_lets_a_message_reach_a_harness_twice() -> Result<(), String> {
    finds(Planted::WaitingUnrecorded, 20_000).map(|_| ())
}

/// CP-T3.2 (the sixth adversarial check's): a move whose answer was lost, its session going on where
/// it was with its fence lifted, lets a message reach a harness twice: the checker finds it (kiln's
/// keeps the fence and waits to hear).
#[test]
fn a_move_whose_answer_was_lost_going_on_where_it_was_lets_a_message_reach_a_harness_twice() -> Result<(), String> {
    finds(Planted::UnknownUnfenced, 20_000).map(|_| ())
}

/// CP-T3.2: a session moving on before it took in the ids it brought, leaving them behind, lets a
/// retry be taken again where it goes: the checker finds it (kiln's carries them too).
#[test]
fn a_move_that_leaves_the_ids_a_session_brought_behind_lets_a_message_reach_a_harness_twice() -> Result<(), String> {
    finds(Planted::WaitingNotCarried, 20_000).map(|_| ())
}
