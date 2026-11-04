//! The connection's server half (docs/browser-protocol.md): sans-IO. A carrier hands it the bytes
//! that came (`receive`) and asks it for bytes to send when it has room (`poll_transmit`); the
//! server's code hears what the browser asked through `poll_event` and answers with `answer`,
//! `start` and `push`. Time and the ticket store are passed in.
//!
//! The send side is the scheduler (plan step 7): priority and coalescing are decided when bytes
//! leave, not when they're queued. Control frames go first (a probe ahead of all), then the
//! subscriptions by priority class, taking turns within a class; a state or telemetry topic's
//! change to a key merges into one to the same key still waiting; text appended to the same place
//! joins; commands, asks, answers and acks are never merged or dropped. Large frames go in
//! fragments of the carrier's bound. Credit is counted per subscription and per connection, and
//! what waits is bounded: past the bound, the largest subscription lets its waiting changes go and
//! is resumed from its cursor once it has room.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;

use crate::answers::{Answers, Taking};
use crate::compress::Compressors;
use crate::conn::{CAP_DEFLATE_RAW, Class, Lane, PRIOS, Prio, Sent, TopicSpec, Transmit, Trust, credit_exempt, info, secret};
use crate::frames::*;
use crate::join::{Joiner, cut};
use crate::liveness::{Liveness, Verdict};
use crate::state::ItemKey;
use crate::tickets::{Principal, Redeem};
use crate::topic::{Published, Start, weight};
use crate::wire::{Bits, Bytes, Why};

/// What the server's code hears from the connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The browser opened, as `principal`, speaking `version`.
    Opened { principal: Principal, version: u32 },
    /// The open was refused: note it (`kiln_wire_refused_total`, wire `bp_up`; the audit log).
    OpenRefused { code: OpenRefusal, why: String },
    /// A frame the browser sent couldn't be read: note it (wire `bp_up`). The browser was told.
    Unreadable { kind: Option<u32>, why: Why },
    /// The browser couldn't read a frame the server sent: note it (wire `bp_down`).
    Unread { kind: u32, code: UnreadCode, why: UnreadWhy },
    /// Run this request, and answer it with `Server::answer`.
    Request { id: u64, call: Call },
    /// Check access to the topic and start the subscription with `Server::start`.
    Subscribe { sub: u32, topic: Topic, cursor: Option<Cursor> },
    /// The subscription ended at the browser's word: stop pushing to it.
    Unsubscribed { sub: u32 },
    /// The subscription fell too far behind and let its waiting changes go: start it again from
    /// `cursor` (`TopicLog::start`), now that it has room.
    Resume { sub: u32, topic: Topic, cursor: Option<Cursor> },
    Input { sub: u32, data: Bytes },
    Resize { sub: u32, cols: u16, rows: u16 },
    /// The connection is over (closed by either side, or its session expired).
    Closed { code: CloseCode, why: String },
}

/// The server half's settings.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub version: u32,
    pub oldest: u32,
    pub caps: Bits,
    pub idle_ms: u32,
    /// The fragment bound to start with (the carrier sets it from its rate: `set_fragment`).
    pub fragment: usize,
    /// What may wait for this client, in bytes.
    pub max_queued: usize,
    pub max_subs: usize,
    pub build: String,
    /// Whether to compress (when the browser offers it).
    pub compress: bool,
    /// The carrier has a stream per subscription: compression contexts are per lane too.
    pub multistream: bool,
}

/// The fragment bound's floor.
pub const MIN_FRAGMENT: usize = 256;

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            version: VERSION,
            oldest: OLDEST,
            caps: Bits(CAP_DEFLATE_RAW),
            idle_ms: 15_000,
            fragment: 1200,
            max_queued: 1 << 20,
            max_subs: 256,
            build: String::new(),
            compress: true,
            multistream: false,
        }
    }
}

/// An item waiting on a subscription.
#[derive(Debug, Clone)]
enum Item {
    Begin { kind: Kind, cursor: Cursor },
    Change { lo: u64, hi: u64, delta: Arc<Delta>, snapshot: bool },
    Ready,
    End { code: EndCode, why: String },
}

fn item_weight(i: &Item) -> usize {
    match i {
        Item::Change { delta, .. } => weight(delta),
        Item::End { why, .. } => 16usize.saturating_add(why.len()),
        Item::Begin { .. } | Item::Ready => 16,
    }
}

/// What text appended goes into: a stream, or one field of one item.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Stream,
    Field(ItemKey, u32),
}

fn target_of(d: &Delta) -> Option<(Target, u64, usize)> {
    match d {
        Delta::Append { at, data } => Some((Target::Stream, *at, data.0.len())),
        Delta::Extend { key, field, at, data } => Some((Target::Field(ItemKey::of(key)?, *field), *at, data.0.len())),
        _ => None,
    }
}

/// The key a state change merges under: the record (None) or an item.
fn merge_key(d: &Delta) -> Option<Option<ItemKey>> {
    match d {
        Delta::Set { .. } => Some(None),
        Delta::Put { key, .. } | Delta::Patch { key, .. } | Delta::Remove { key } => ItemKey::of(key).map(Some),
        Delta::Append { .. } | Delta::Extend { .. } => None,
    }
}

/// `b` merged over `a` (a waiting change and a newer one to the same key), when they merge.
fn merge(a: &Delta, b: &Delta) -> Option<Delta> {
    use crate::state::{merge_fields, set_fields};
    match (a, b) {
        (Delta::Set { mask: am, fields: af }, Delta::Set { mask: bm, fields: bf }) => {
            let (mask, fields) = merge_fields(*am, af, *bm, bf);
            Some(Delta::Set { mask, fields })
        }
        (Delta::Put { key, fields }, Delta::Patch { mask, fields: pf, .. }) => {
            let mut all = fields.clone();
            set_fields(&mut all, *mask, pf);
            Some(Delta::Put { key: key.clone(), fields: all })
        }
        (Delta::Patch { key, mask: am, fields: af }, Delta::Patch { mask: bm, fields: bf, .. }) => {
            let (mask, fields) = merge_fields(*am, af, *bm, bf);
            Some(Delta::Patch { key: key.clone(), mask, fields })
        }
        (_, Delta::Remove { .. }) | (Delta::Remove { .. }, Delta::Put { .. }) | (_, Delta::Put { .. }) => Some(b.clone()),
        _ => None,
    }
}

/// Text appended to the same place, joined: `a` then `b`, when `b` starts where `a` ends.
fn join(a: &Delta, b: &Delta, most: usize) -> Option<Delta> {
    let (ta, at_a, la) = target_of(a)?;
    let (tb, at_b, lb) = target_of(b)?;
    if ta != tb || at_a.saturating_add(u64::try_from(la).ok()?) != at_b || la.saturating_add(lb) > most {
        return None;
    }
    match (a, b) {
        (Delta::Append { at, data }, Delta::Append { data: more, .. }) => {
            let mut d = data.0.clone();
            d.extend_from_slice(&more.0);
            Some(Delta::Append { at: *at, data: Bytes(d) })
        }
        (Delta::Extend { key, field, at, data }, Delta::Extend { data: more, .. }) => {
            let mut d = data.0.clone();
            d.extend_from_slice(&more.0);
            Some(Delta::Extend { key: key.clone(), field: *field, at: *at, data: Bytes(d) })
        }
        _ => None,
    }
}

/// How many `Str` keys a subscription's alias table holds at most.
pub const MAX_ALIASES: usize = 4096;

#[derive(Debug)]
struct Sub {
    topic: Topic,
    spec: TopicSpec,
    items: VecDeque<Item>,
    /// The sequence number of `items`' first.
    head: u64,
    next: u64,
    /// Waiting state changes by key, by their sequence number.
    index: HashMap<Option<ItemKey>, u64>,
    /// Encoded frames of this lane waiting to go (fragments, or a frame past the room).
    pending: VecDeque<(Kind, Vec<u8>)>,
    queued: usize,
    /// The newest revision waiting.
    queued_hi: u64,
    /// The revision of the last change sent: the browser's cursor once `ready`.
    through: u64,
    /// Whether the browser has a cursor (a snapshot or resume finished).
    ready: bool,
    /// Whether a snapshot's items lead what's waiting.
    in_snapshot: bool,
    credit: u64,
    sent: u64,
    aliases: HashMap<String, u32>,
    tail: Option<(Target, u64)>,
    /// Let go of what waited: to be resumed.
    behind: bool,
    /// Resume asked for, not yet started.
    resuming: bool,
    ended: bool,
}

impl Sub {
    fn prio(&self) -> Prio {
        if self.in_snapshot { Prio::Bulk } else { self.spec.prio }
    }

    fn has_work(&self) -> bool {
        !self.pending.is_empty() || !self.items.is_empty()
    }

    fn credit_left(&self) -> u64 {
        if self.credit == 0 { u64::MAX } else { self.credit.saturating_sub(self.sent) }
    }

    fn push_item(&mut self, item: Item) {
        self.queued = self.queued.saturating_add(item_weight(&item));
        self.items.push_back(item);
        self.next = self.next.saturating_add(1);
    }

    fn pop_item(&mut self) -> Option<Item> {
        let item = self.items.pop_front()?;
        self.queued = self.queued.saturating_sub(item_weight(&item));
        let seq = self.head;
        self.head = self.head.saturating_add(1);
        self.index.retain(|_, s| *s != seq);
        Some(item)
    }

    fn let_go(&mut self) -> usize {
        let freed = self.queued;
        self.items.clear();
        self.index.clear();
        self.head = self.next;
        self.queued = 0;
        self.in_snapshot = false;
        freed
    }
}

/// Where the connection is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    /// Waiting for `Open`.
    Opening,
    Open,
    /// Over: what's left to send goes, then nothing.
    Closed,
}

/// The server half of one browser's connection.
#[derive(Debug)]
pub struct Server {
    cfg: ServerConfig,
    /// The connection's id among the server's (for answers waited on across connections).
    id: u64,
    born: u64,
    phase: Phase,
    version: u32,
    caps: Bits,
    principal: Principal,
    live: Liveness,
    /// When the browser last did something (idle frames aside).
    activity: u64,
    /// When this side last sent anything.
    last_sent: u64,
    events: VecDeque<Event>,
    control: VecDeque<(Kind, Vec<u8>, Trust)>,
    /// A probe to send, and the one out (its nonce and when it went).
    probe_want: bool,
    probe_out: Option<(u32, u64)>,
    acks: VecDeque<ProbeAck>,
    /// Subscriptions passed over in this transmit (no credit, or nothing that fits).
    skip: std::collections::BTreeSet<u32>,
    idle_due: bool,
    subs: BTreeMap<u32, Sub>,
    /// Round robin within each class: the last sub served.
    turn: [u32; 5],
    joiner: Joiner,
    fragment: usize,
    conn_credit: u64,
    conn_sent: u64,
    /// Bytes received and sent, all of them (the positions idle frames carry).
    got: u64,
    put: u64,
    nonce: u32,
    compress: Option<Compressors>,
    /// Requests this connection waits on, by id.
    waiting: BTreeMap<u64, ()>,
}

impl Server {
    /// A connection, `id` among the server's, of the server incarnation `born`, that came at `now`.
    pub fn new(cfg: ServerConfig, id: u64, born: u64, now: u64) -> Self {
        let live = Liveness::new(u64::from(cfg.idle_ms), now);
        let fragment = cfg.fragment.max(MIN_FRAGMENT);
        Self {
            cfg,
            id,
            born,
            phase: Phase::Opening,
            version: VERSION,
            caps: Bits(0),
            principal: Principal::default(),
            live,
            activity: now,
            last_sent: now,
            events: VecDeque::new(),
            control: VecDeque::new(),
            probe_want: false,
            probe_out: None,
            acks: VecDeque::new(),
            skip: std::collections::BTreeSet::new(),
            idle_due: false,
            subs: BTreeMap::new(),
            turn: [0; 5],
            joiner: Joiner::new(),
            fragment,
            conn_credit: 0,
            conn_sent: 0,
            got: 0,
            put: 0,
            nonce: 0,
            compress: None,
            waiting: BTreeMap::new(),
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    pub fn version(&self) -> u32 {
        self.version
    }

    pub fn is_open(&self) -> bool {
        self.phase == Phase::Open
    }

    pub fn is_closed(&self) -> bool {
        self.phase == Phase::Closed
    }

    /// When the browser last did something: idle frames, probes and their answers aren't
    /// activity (plan 01a19fa90f92).
    pub fn activity(&self) -> u64 {
        self.activity
    }

    pub fn liveness(&self) -> &Liveness {
        &self.live
    }

    /// The carrier's fragment bound, from the rate it measures.
    pub fn set_fragment(&mut self, bytes: usize) {
        self.fragment = bytes.max(MIN_FRAGMENT);
    }

    /// Bytes waiting for this client.
    pub fn queued(&self) -> usize {
        self.subs.values().map(|s| s.queued.saturating_add(s.pending.iter().map(|(_, b)| b.len()).sum::<usize>())).sum::<usize>().saturating_add(self.control.iter().map(|(_, b, _)| b.len()).sum::<usize>())
    }

    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    fn queue_control(&mut self, f: Frame, trust: Trust) {
        if f.kind().since() > self.version {
            return;
        }
        let b = f.encode(self.version);
        self.control.push_back((f.kind(), b, trust));
    }

    fn refuse_frame(&mut self, kind: Option<u32>, why: Why) {
        self.events.push_back(Event::Unreadable { kind, why });
        if self.phase == Phase::Open {
            let code = match why.code() {
                crate::wire::Code::Unknown => UnreadCode::Unknown,
                crate::wire::Code::Malformed => UnreadCode::Malformed,
            };
            self.queue_control(Frame::Unread(Unread { kind: kind.unwrap_or(u32::MAX), code, why: UnreadWhy::of(why) }), Trust::Secret);
        }
    }

    /// Closes the connection, telling the browser why.
    pub fn close(&mut self, code: CloseCode, why: &str, build: &str) {
        if self.phase == Phase::Closed {
            return;
        }
        if self.phase == Phase::Open {
            self.queue_control(Frame::Close(Close { code: code.clone(), why: why.into(), build: build.into() }), Trust::Secret);
        }
        self.phase = Phase::Closed;
        self.events.push_back(Event::Closed { code, why: why.into() });
    }

    /// The bytes that came, at `now`; `tickets` redeems the open's ticket, `answers` takes each
    /// request once per id across the caller's connections.
    pub fn receive(&mut self, bytes: &[u8], now: u64, tickets: &mut dyn Redeem, answers: &mut Answers) {
        if bytes.is_empty() || self.phase == Phase::Closed {
            return;
        }
        self.got = self.got.saturating_add(u64::try_from(bytes.len()).unwrap_or(0));
        self.live.arrived(now);
        let mut frames = Frames::new(bytes);
        while let Some(raw) = frames.next_raw() {
            let read = raw.and_then(|(kind, body)| decode_body(kind, body));
            match read {
                Ok(r) => self.take(r.frame, now, tickets, answers),
                Err(e) => {
                    if self.phase == Phase::Opening {
                        self.refuse_open(OpenRefusal::Malformed, "the first frame isn't an open that reads");
                        return;
                    }
                    self.refuse_frame(e.kind, e.why);
                }
            }
            if self.phase == Phase::Closed {
                return;
            }
        }
        self.expire(now);
    }

    fn refuse_open(&mut self, code: OpenRefusal, why: &str) {
        let f = Frame::Refused(Refused { code: code.clone(), why: why.into(), build: self.cfg.build.clone() });
        self.control.push_back((Kind::Refused, f.encode(self.cfg.oldest), Trust::Secret));
        self.events.push_back(Event::OpenRefused { code, why: why.into() });
        self.phase = Phase::Closed;
        self.events.push_back(Event::Closed { code: CloseCode::Bye, why: why.into() });
    }

    fn open(&mut self, o: Open, now: u64, tickets: &mut dyn Redeem) {
        // The version both speak: the newer oldest up to the older newest.
        let lo = o.oldest.max(self.cfg.oldest);
        let hi = o.version.min(self.cfg.version);
        if o.version < self.cfg.oldest {
            return self.refuse_open(OpenRefusal::TooOld, "this page's build is older than the server serves: switch builds");
        }
        if o.oldest > self.cfg.version || lo > hi {
            return self.refuse_open(OpenRefusal::TooNew, "this page's build is newer than the server");
        }
        if hi == self.cfg.version && o.version == self.cfg.version && o.schema.0 != schema_hash() {
            return self.refuse_open(OpenRefusal::Schema, "the page's codec and the server's disagree: switch builds");
        }
        let principal = match tickets.redeem(&o.ticket.0, now) {
            Ok(p) => p,
            Err(code) => {
                let why = match code {
                    OpenRefusal::Expired => "the ticket is past its life",
                    OpenRefusal::Reused => "the ticket was taken already",
                    _ => "the ticket isn't one this server made",
                };
                return self.refuse_open(code, why);
            }
        };
        self.version = hi;
        self.caps = Bits(o.caps.0 & self.cfg.caps.0);
        self.principal = principal.clone();
        self.conn_credit = o.window;
        self.live.set_idle(u64::from(o.idle_ms));
        self.phase = Phase::Open;
        self.activity = now;
        if self.cfg.compress && self.caps.0 & CAP_DEFLATE_RAW != 0 {
            self.compress = Some(Compressors::new(self.cfg.multistream));
        } else {
            self.caps = Bits(self.caps.0 & !CAP_DEFLATE_RAW);
        }
        let accepted = Frame::Accepted(Accepted { version: hi, caps: self.caps, idle_ms: self.cfg.idle_ms, born: self.born, fragment: u32::try_from(self.fragment).unwrap_or(u32::MAX), build: self.cfg.build.clone() });
        self.queue_control(accepted, Trust::Secret);
        // A probe with the accept measures the first round trip.
        self.send_probe(now);
        self.events.push_back(Event::Opened { principal, version: hi });
    }

    /// Asks for a probe, unless one is out for this silence already.
    fn send_probe(&mut self, _now: u64) {
        if self.probe_out.is_some() && self.live.probing() {
            return;
        }
        self.probe_want = true;
    }

    fn take(&mut self, f: Frame, now: u64, tickets: &mut dyn Redeem, answers: &mut Answers) {
        if self.phase == Phase::Opening {
            match f {
                Frame::Open(o) => self.open(o, now, tickets),
                _ => self.refuse_open(OpenRefusal::Malformed, "the first frame wasn't an open"),
            }
            return;
        }
        if !matches!(f, Frame::Idle(_) | Frame::Probe(_) | Frame::ProbeAck(_)) {
            self.activity = now;
        }
        match f {
            Frame::Open(_) | Frame::Accepted(_) | Frame::Refused(_) | Frame::Answer(_) | Frame::SecretAnswer(_) | Frame::Snapshot(_) | Frame::Resumed(_) | Frame::Change(_) | Frame::Tail(_) | Frame::Ready(_) | Frame::End(_) | Frame::Compressed(_) => {
                let k = f.kind().index();
                self.refuse_frame(Some(k), Why::OutOfOrder);
            }
            Frame::Request(r) => self.request(r, now, answers),
            Frame::Subscribe(s) => self.subscribe(s),
            Frame::Unsubscribe(u) => {
                if let Some(s) = self.subs.get_mut(&u.sub) {
                    s.let_go();
                    s.pending.clear();
                    s.ended = true;
                    s.push_item(Item::End { code: EndCode::Done, why: String::new() });
                    self.events.push_back(Event::Unsubscribed { sub: u.sub });
                }
            }
            Frame::Credit(c) => self.credit(c),
            Frame::Fragment(fr) => match self.joiner.take(fr) {
                None => {}
                Some(Ok(r)) => self.take(r.frame, now, tickets, answers),
                Some(Err(e)) => self.refuse_frame(e.kind, e.why),
            },
            Frame::Idle(_) => {}
            Frame::Probe(p) => self.acks.push_back(ProbeAck { nonce: p.nonce, got: self.got }),
            Frame::ProbeAck(a) => {
                if let Some((n, at)) = self.probe_out {
                    if n == a.nonce {
                        self.live.round_trip(now.saturating_sub(at));
                        self.probe_out = None;
                    }
                }
            }
            Frame::Unread(u) => self.events.push_back(Event::Unread { kind: u.kind, code: u.code, why: u.why }),
            Frame::Close(c) => {
                self.phase = Phase::Closed;
                self.events.push_back(Event::Closed { code: c.code, why: c.why });
            }
            Frame::Input(i) => self.events.push_back(Event::Input { sub: i.sub, data: i.data }),
            Frame::Resize(r) => self.events.push_back(Event::Resize { sub: r.sub, cols: r.cols, rows: r.rows }),
        }
    }

    fn request(&mut self, r: Request, now: u64, answers: &mut Answers) {
        match answers.begin(&self.principal.who, r.id, self.id, now) {
            Taking::Run => {
                self.waiting.insert(r.id, ());
                self.events.push_back(Event::Request { id: r.id, call: r.call });
            }
            Taking::Wait => {
                self.waiting.insert(r.id, ());
            }
            Taking::Kept { outcome, at } => {
                if outcome.finality() == Finality::ForNow {
                    self.waiting.insert(r.id, ());
                }
                self.queue_answer(r.id, outcome, at, Trust::Secret, false);
            }
        }
    }

    fn queue_answer(&mut self, id: u64, outcome: Outcome, at: Option<Mark>, trust: Trust, is_secret: bool) {
        let body = Answer { id, outcome, at };
        if is_secret {
            self.queue_control(Frame::SecretAnswer(body), Trust::Secret);
        } else {
            self.queue_control(Frame::Answer(body), trust);
        }
    }

    /// A request's answer, from the server's code: kept under its id for the caller, and sent to
    /// every connection of theirs that waits for it (`answers.finish` names them; this one sends
    /// it when it's among them). `trust` is the route's class (`Trust::Secret` when it carries a
    /// secret or isn't classed: never compressed).
    pub fn answer(&mut self, id: u64, outcome: Outcome, at: Option<Mark>, trust: Trust, answers: &mut Answers, now: u64) -> Vec<u64> {
        let to = answers.finish(&self.principal.who, id, &outcome, at.as_ref(), now);
        if to.contains(&self.id) {
            self.deliver(id, outcome, at, trust);
        }
        to
    }

    /// An answer `Answers::finish` said this connection waits for.
    pub fn deliver(&mut self, id: u64, outcome: Outcome, at: Option<Mark>, trust: Trust) {
        if outcome.finality() != Finality::ForNow {
            self.waiting.remove(&id);
        }
        self.queue_answer(id, outcome, at, trust, false);
    }

    /// An answer whose body carries a secret: a `SecretAnswer`, never compressed.
    pub fn answer_secret(&mut self, id: u64, outcome: Outcome, answers: &mut Answers, now: u64) {
        let to = answers.finish(&self.principal.who, id, &outcome, None, now);
        if to.contains(&self.id) {
            self.waiting.remove(&id);
            self.queue_answer(id, outcome, None, Trust::Secret, true);
        }
    }

    fn subscribe(&mut self, s: Subscribe) {
        if s.sub == 0 {
            return self.refuse_frame(Some(Kind::Subscribe.index()), Why::BadValue);
        }
        let live = self.subs.values().filter(|x| !x.ended).count();
        if live >= self.cfg.max_subs && !self.subs.contains_key(&s.sub) {
            let f = Frame::End(End { sub: s.sub, code: EndCode::Limit, why: "too many subscriptions".into() });
            self.queue_control(f, Trust::Secret);
            return;
        }
        // A subscription again under its id starts over.
        self.subs.remove(&s.sub);
        let spec = TopicSpec { kind: s.topic.kind, class: Class::State, prio: Prio::State, trust: Trust::Secret };
        let sub = Sub {
            topic: s.topic.clone(),
            spec,
            items: VecDeque::new(),
            head: 0,
            next: 0,
            index: HashMap::new(),
            pending: VecDeque::new(),
            queued: 0,
            queued_hi: 0,
            through: 0,
            ready: false,
            in_snapshot: false,
            credit: s.credit,
            sent: 0,
            aliases: HashMap::new(),
            tail: None,
            behind: false,
            resuming: true,
            ended: false,
        };
        self.subs.insert(s.sub, sub);
        self.events.push_back(Event::Subscribe { sub: s.sub, topic: s.topic, cursor: s.cursor });
    }

    fn credit(&mut self, c: Credit) {
        if c.sub == 0 {
            if c.upto < self.conn_credit {
                return self.close(CloseCode::Protocol, "credit taken back", "");
            }
            self.conn_credit = c.upto;
            return;
        }
        let mut back = false;
        if let Some(s) = self.subs.get_mut(&c.sub) {
            if c.upto < s.credit {
                back = true;
            } else {
                s.credit = c.upto;
            }
        }
        if back {
            self.close(CloseCode::Protocol, "credit taken back", "");
        }
    }

    /// Starts a subscription the server's code allowed (or refuses it): its spec from the
    /// catalogue, and how it starts (`TopicLog::start`).
    pub fn start(&mut self, sub: u32, spec: TopicSpec, how: Start) {
        let born = self.born;
        let Some(s) = self.subs.get_mut(&sub) else { return };
        s.spec = spec;
        s.resuming = false;
        s.behind = false;
        s.let_go();
        match how {
            Start::Refuse { code, why } => {
                s.ended = true;
                s.push_item(Item::End { code, why });
            }
            Start::Snapshot { cursor, items } => {
                s.ready = false;
                s.in_snapshot = true;
                s.push_item(Item::Begin { kind: Kind::Snapshot, cursor: Cursor { born, rev: cursor.rev } });
                for d in items {
                    s.push_item(Item::Change { lo: cursor.rev, hi: cursor.rev, delta: Arc::new(d), snapshot: true });
                }
                s.push_item(Item::Ready);
                s.queued_hi = cursor.rev;
            }
            Start::Resume { from, changes } => {
                s.ready = false;
                s.push_item(Item::Begin { kind: Kind::Resumed, cursor: from.clone() });
                s.queued_hi = from.rev;
                for p in changes {
                    Self::enqueue(s, &p);
                }
                s.push_item(Item::Ready);
            }
        }
        self.bound();
    }

    /// A topic's change, for a subscription to it.
    pub fn push(&mut self, sub: u32, p: &Published) {
        let Some(s) = self.subs.get_mut(&sub) else { return };
        if s.behind || s.resuming || s.ended || p.rev <= s.queued_hi {
            return;
        }
        Self::enqueue(s, p);
        self.bound();
    }

    /// The topic ended: its subscription ends once what waits has gone.
    pub fn end(&mut self, sub: u32, code: EndCode, why: &str) {
        if let Some(s) = self.subs.get_mut(&sub) {
            s.ended = true;
            s.push_item(Item::End { code, why: why.into() });
        }
    }

    fn enqueue(s: &mut Sub, p: &Published) {
        s.queued_hi = s.queued_hi.max(p.rev);
        let d = &p.delta;
        if matches!(s.spec.class, Class::State | Class::Telemetry) {
            if let Some(key) = merge_key(d) {
                if let Some(seq) = s.index.get(&key).copied() {
                    let at = usize::try_from(seq.saturating_sub(s.head)).unwrap_or(usize::MAX);
                    if let Some(Item::Change { hi, delta, snapshot: false, .. }) = s.items.get_mut(at) {
                        if let Some(m) = merge(delta, d) {
                            let before = weight(delta);
                            *delta = Arc::new(m);
                            *hi = p.rev;
                            s.queued = s.queued.saturating_sub(before).saturating_add(weight(delta));
                            return;
                        }
                    }
                }
                let seq = s.next;
                s.push_item(Item::Change { lo: p.rev, hi: p.rev, delta: d.clone(), snapshot: false });
                s.index.insert(key, seq);
                return;
            }
        }
        // Text appended to where the last waiting change left off joins it (up to a fragment's worth).
        if let Some(Item::Change { hi, delta, snapshot: false, .. }) = s.items.back_mut() {
            if let Some(j) = join(delta, d, 1024) {
                let before = weight(delta);
                *delta = Arc::new(j);
                *hi = p.rev;
                s.queued = s.queued.saturating_sub(before).saturating_add(weight(delta));
                return;
            }
        }
        s.push_item(Item::Change { lo: p.rev, hi: p.rev, delta: d.clone(), snapshot: false });
    }

    /// Keeps what waits within the bound: the largest subscription lets its waiting changes go,
    /// to be resumed from its cursor once it has room.
    fn bound(&mut self) {
        while self.queued() > self.cfg.max_queued {
            let Some((&id, _)) = self.subs.iter().filter(|(_, s)| s.queued > 0 && !s.ended).max_by_key(|(_, s)| s.queued) else { return };
            if let Some(s) = self.subs.get_mut(&id) {
                s.let_go();
                s.behind = true;
                s.tail = None;
            }
        }
    }

    fn expire(&mut self, now: u64) {
        if self.phase == Phase::Open {
            if let Some(at) = self.principal.expires_at {
                if now >= at {
                    self.close(CloseCode::Expired, "the browser session ended", "");
                }
            }
        }
    }

    /// The link's verdict now (from when bytes arrived); a suspect link gets a probe.
    pub fn verdict(&mut self, now: u64) -> Verdict {
        let v = self.live.verdict(now);
        if v == Verdict::Suspect && self.phase == Phase::Open {
            self.send_probe(now);
        }
        v
    }

    /// When the connection next needs looking at: an idle frame due, the verdict changing, the
    /// session expiring.
    pub fn poll_timeout(&self, now: u64) -> u64 {
        let idle = self.last_sent.saturating_add(u64::from(self.cfg.idle_ms));
        let mut t = idle.min(self.live.next_change(now));
        if let Some(at) = self.principal.expires_at {
            t = t.min(at);
        }
        t
    }

    /// Bytes to send, at most `room`, by priority; `None` when nothing is due.
    pub fn poll_transmit(&mut self, room: usize, now: u64) -> Option<Transmit> {
        self.expire(now);
        if self.phase == Phase::Open {
            let _ = self.verdict(now);
            if now.saturating_sub(self.last_sent) >= u64::from(self.cfg.idle_ms) && self.control.is_empty() && !self.subs.values().any(Sub::has_work) {
                self.idle_due = true;
            }
            self.wake_behind();
        }
        let mut parts: Vec<Part> = Vec::new();
        let mut used = 0usize;
        let slack = if self.compress.is_some() { 16 } else { 0 };
        let room = room.saturating_sub(slack);
        // A probe and its answers go ahead of everything.
        if self.probe_want && self.phase == Phase::Open {
            let nonce = self.nonce.wrapping_add(1);
            let b = Frame::Probe(Probe { nonce }).encode(self.version);
            if b.len() <= room {
                self.nonce = nonce;
                self.probe_want = false;
                self.probe_out = Some((nonce, now));
                self.live.probe_sent(now);
                used = used.saturating_add(b.len());
                parts.push(Part { kind: Kind::Probe, lane: Lane::Datagram, prio: Prio::Control, bytes: b, trust: Trust::Secret, urgent: true });
            }
        }
        while let Some(a) = self.acks.front().cloned() {
            let b = Frame::ProbeAck(a).encode(self.version);
            if used.saturating_add(b.len()) > room {
                break;
            }
            self.acks.pop_front();
            used = used.saturating_add(b.len());
            parts.push(Part { kind: Kind::ProbeAck, lane: Lane::Datagram, prio: Prio::Control, bytes: b, trust: Trust::Secret, urgent: true });
        }
        // The control lane.
        while let Some((kind, b, _)) = self.control.front() {
            let (kind, len) = (*kind, b.len());
            if len > self.fragment {
                if let Some((k, b, t)) = self.control.pop_front() {
                    let frags = cut(0, &b, self.fragment);
                    for f in frags.into_iter().rev() {
                        self.control.push_front((Kind::Fragment, f.encode(self.version), t));
                    }
                    let _ = k;
                }
                continue;
            }
            if used.saturating_add(len) > room {
                break;
            }
            if !credit_exempt(kind) && self.conn_sent.saturating_add(len as u64) > self.conn_credit {
                break;
            }
            let Some((kind, b, trust)) = self.control.pop_front() else { break };
            if !credit_exempt(kind) {
                self.conn_sent = self.conn_sent.saturating_add(b.len() as u64);
            }
            used = used.saturating_add(b.len());
            let urgent = info(kind).urgent == "yes";
            parts.push(Part { kind, lane: Lane::Control, prio: Prio::Control, bytes: b, trust, urgent });
        }
        // The subscriptions, by class, taking turns within one.
        if self.phase != Phase::Opening {
            'classes: for prio in PRIOS {
                while let Some(id) = self.next_sub(prio) {
                    match self.take_sub(id, room.saturating_sub(used), used == 0) {
                        Took::Part(p) => {
                            used = used.saturating_add(p.bytes.len());
                            parts.push(p);
                        }
                        // The room is full: what's next in order waits for the next.
                        Took::NoRoom => break 'classes,
                        Took::NoCredit | Took::Empty => {
                            self.skip.insert(id);
                        }
                    }
                }
            }
            self.skip.clear();
        }
        if parts.is_empty() && self.idle_due && self.phase == Phase::Open {
            let b = Frame::Idle(Idle { got: self.got }).encode(self.version);
            if b.len() <= room {
                parts.push(Part { kind: Kind::Idle, lane: Lane::Datagram, prio: Prio::Control, bytes: b, trust: Trust::Secret, urgent: false });
            }
        }
        self.idle_due = false;
        if parts.is_empty() {
            return None;
        }
        let t = self.assemble(parts);
        self.put = self.put.saturating_add(t.bytes.len() as u64);
        self.last_sent = now;
        Some(t)
    }

    /// The next subscription of `prio` with something to send, taking turns.
    fn next_sub(&self, prio: Prio) -> Option<u32> {
        let after = self.turn.get(prio as usize).copied().unwrap_or(0);
        let ready = |(id, s): (&u32, &Sub)| -> Option<u32> { (s.has_work() && s.prio() == prio && !self.skip.contains(id)).then_some(*id) };
        self.subs.range(after.saturating_add(1)..).find_map(|(id, s)| ready((id, s))).or_else(|| self.subs.range(..=after).find_map(|(id, s)| ready((id, s))))
    }

    /// The next frame of subscription `id`, if it fits `room` and its credit; `first` when
    /// nothing is in the transmit yet (a frame past the room is then cut to it).
    fn take_sub(&mut self, id: u32, room: usize, first: bool) -> Took {
        let version = self.version;
        let fragment = self.fragment;
        let born = self.born;
        let conn_left = self.conn_credit.saturating_sub(self.conn_sent);
        let Some(s) = self.subs.get_mut(&id) else { return Took::Empty };
        if s.pending.is_empty() {
            let Some(frame) = Self::encode_next(s, id, born) else { return Took::Empty };
            let b = frame.encode(version);
            let kind = frame.kind();
            if b.len() > fragment {
                for f in cut(id, &b, fragment) {
                    s.pending.push_back((Kind::Fragment, f.encode(version)));
                }
            } else {
                s.pending.push_back((kind, b));
            }
        }
        let Some((kind, len)) = s.pending.front().map(|(k, b)| (*k, b.len())) else { return Took::Empty };
        if len > room {
            // A carrier with less room than a fragment: cut what's next to fit it.
            if first && room >= 64 && kind != Kind::Fragment {
                if let Some((_, b)) = s.pending.pop_front() {
                    for f in cut(id, &b, room).into_iter().rev() {
                        s.pending.push_front((Kind::Fragment, f.encode(version)));
                    }
                }
                return self.take_sub(id, room, false);
            }
            return Took::NoRoom;
        }
        if !credit_exempt(kind) && (s.credit_left() < len as u64 || conn_left < len as u64) {
            return Took::NoCredit;
        }
        let Some((kind, b)) = s.pending.pop_front() else { return Took::Empty };
        if !credit_exempt(kind) {
            s.sent = s.sent.saturating_add(b.len() as u64);
            self.conn_sent = self.conn_sent.saturating_add(b.len() as u64);
        }
        let prio = if kind == Kind::Snapshot || s.in_snapshot { Prio::Bulk } else { s.spec.prio };
        let urgent = s.spec.prio == Prio::Interactive && matches!(kind, Kind::Change | Kind::Tail | Kind::Fragment);
        let trust = if kind == Kind::End { Trust::Secret } else { s.spec.trust };
        if let Some(t) = self.turn.get_mut(prio as usize) {
            *t = id;
        }
        if let Some(t) = self.turn.get_mut(s.spec.prio as usize) {
            *t = id;
        }
        Took::Part(Part { kind, lane: Lane::Sub(id), prio, bytes: b, trust, urgent })
    }

    /// Takes the next item of a subscription and makes its frame: the revision through which the
    /// browser then has everything, its step, keys by alias, appends as `Tail` where they can be.
    fn encode_next(s: &mut Sub, id: u32, born: u64) -> Option<Frame> {
        let item = s.pop_item()?;
        Some(match item {
            Item::Begin { kind, cursor } => {
                s.aliases.clear();
                s.tail = None;
                s.through = cursor.rev;
                let b = Begin { sub: id, cursor: Cursor { born, rev: cursor.rev } };
                if kind == Kind::Snapshot { Frame::Snapshot(b) } else { Frame::Resumed(b) }
            }
            Item::Ready => {
                s.ready = true;
                s.in_snapshot = false;
                Frame::Ready(Begin { sub: id, cursor: Cursor { born, rev: s.through } })
            }
            Item::End { code, why } => Frame::End(End { sub: id, code, why }),
            Item::Change { lo: _, hi, delta, snapshot } => {
                let (rev, step) = if snapshot {
                    (hi, 0)
                } else {
                    let next_lo = s.items.iter().find_map(|i| match i {
                        Item::Change { lo, snapshot: false, .. } => Some(*lo),
                        _ => None,
                    });
                    let through = next_lo.map_or(s.queued_hi, |lo| lo.saturating_sub(1)).max(s.through.saturating_add(1));
                    let step = through.saturating_sub(s.through);
                    s.through = through;
                    (through, step)
                };
                let target = target_of(&delta);
                if let Some((t, at, len)) = &target {
                    if step == 1 && s.tail.as_ref().is_some_and(|(tt, end)| tt == t && end == at) {
                        s.tail = Some((t.clone(), at.saturating_add(*len as u64)));
                        let data = match delta.as_ref() {
                            Delta::Append { data, .. } | Delta::Extend { data, .. } => data.clone(),
                            _ => Bytes::default(),
                        };
                        return Some(Frame::Tail(Tail { sub: id, data }));
                    }
                }
                s.tail = target.map(|(t, at, len)| (t, at.saturating_add(len as u64)));
                let delta = Self::alias(s, delta.as_ref());
                Frame::Change(Change { sub: id, rev, step, delta })
            }
        })
    }

    /// A delta with its `Str` keys sent by alias where the subscription has sent them before.
    fn alias(s: &mut Sub, d: &Delta) -> Delta {
        let mut key_of = |k: &Key| -> Key {
            match k {
                Key::Str { s: name } => {
                    if let Some(n) = s.aliases.get(name) {
                        Key::Ref { n: *n }
                    } else {
                        if s.aliases.len() < MAX_ALIASES {
                            let n = u32::try_from(s.aliases.len()).unwrap_or(u32::MAX);
                            s.aliases.insert(name.clone(), n);
                        }
                        k.clone()
                    }
                }
                _ => k.clone(),
            }
        };
        match d {
            Delta::Put { key, fields } => Delta::Put { key: key_of(key), fields: fields.clone() },
            Delta::Patch { key, mask, fields } => Delta::Patch { key: key_of(key), mask: *mask, fields: fields.clone() },
            Delta::Remove { key } => Delta::Remove { key: key_of(key) },
            Delta::Extend { key, field, at, data } => Delta::Extend { key: key_of(key), field: *field, at: *at, data: data.clone() },
            Delta::Set { .. } | Delta::Append { .. } => d.clone(),
        }
    }

    /// Subscriptions that let what waited go, now with room: their resume is asked for.
    fn wake_behind(&mut self) {
        let born = self.born;
        let mut wake = Vec::new();
        for (id, s) in &mut self.subs {
            if s.behind && !s.resuming && !s.has_work() && s.credit_left() > 0 {
                s.resuming = true;
                let cursor = s.ready.then_some(Cursor { born, rev: s.through });
                wake.push(Event::Resume { sub: *id, topic: s.topic.clone(), cursor });
            }
        }
        self.events.extend(wake);
    }

    /// The parts as one carrier message: compressible frames batched into their context's
    /// `Compressed` frame where compression is on.
    fn assemble(&mut self, parts: Vec<Part>) -> Transmit {
        let mut t = Transmit::default();
        let version = self.version;
        let mut i = 0;
        while i < parts.len() {
            let Some(p) = parts.get(i) else { break };
            let ctx = if p.kind == Kind::Fragment || secret(p.kind) { None } else { p.trust.ctx() };
            if let (Some(c), Some(z)) = (ctx, self.compress.as_mut()) {
                // A run of frames of one context (and one lane, where lanes are streams).
                let lane = p.lane;
                let mut j = i;
                let mut raw = Vec::new();
                let (mut prio, mut urgent) = (p.prio, false);
                while let Some(q) = parts.get(j) {
                    let qc = if q.kind == Kind::Fragment || secret(q.kind) { None } else { q.trust.ctx() };
                    if qc != Some(c) || (z.multistream() && q.lane != lane) {
                        break;
                    }
                    raw.extend_from_slice(&q.bytes);
                    prio = prio.min(q.prio);
                    urgent |= q.urgent;
                    j += 1;
                }
                let data = z.deflate(c, lane, &raw);
                let len = u32::try_from(raw.len()).unwrap_or(u32::MAX);
                let b = Frame::Compressed(Compressed { ctx: c, len, data: Bytes(data) }).encode(version);
                t.frames.push(Sent { kind: Kind::Compressed, lane, prio, urgent, start: t.bytes.len(), len: b.len() });
                t.bytes.extend_from_slice(&b);
                i = j;
                continue;
            }
            t.frames.push(Sent { kind: p.kind, lane: p.lane, prio: p.prio, urgent: p.urgent, start: t.bytes.len(), len: p.bytes.len() });
            t.bytes.extend_from_slice(&p.bytes);
            i += 1;
        }
        t
    }
}

/// What a subscription had for a transmit.
#[derive(Debug)]
enum Took {
    Part(Part),
    NoRoom,
    NoCredit,
    Empty,
}

/// A frame chosen for a transmit.
#[derive(Debug)]
struct Part {
    kind: Kind,
    lane: Lane,
    prio: Prio,
    bytes: Vec<u8>,
    trust: Trust,
    urgent: bool,
}
