//! The connection's client half (docs/browser-protocol.md), sans-IO: what a browser's core does,
//! in Rust, for tests and tools (the browser's own is `ui/src/api/bp/client.ts`, the same rules).
//! It opens, sends requests and keeps them until a final answer, subscribes from its cursors,
//! notices gaps and resumes, joins fragments, inflates compressed batches, grants credit, and
//! keeps liveness by bytes arriving.

use std::collections::{BTreeMap, VecDeque};

use crate::compress::Inflaters;
use crate::conn::{CAP_DEFLATE_RAW, Lane, Prio, Sent, Transmit};
use crate::frames::*;
use crate::join::{Joiner, cut, read_one};
use crate::liveness::{Liveness, Verdict};
use crate::server::{MAX_ALIASES, MIN_FRAGMENT};
use crate::state::{ItemKey, Misfit, TopicState, text_len};
use crate::wire::{Bits, Bytes, Why};

#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub version: u32,
    pub oldest: u32,
    pub caps: Bits,
    pub idle_ms: u32,
    /// The connection's credit, granted at open and kept topped up while `auto_credit`.
    pub window: u64,
    pub auto_credit: bool,
    pub build: String,
    /// Keep each subscription's state (a `TopicState`), as tests and tools want.
    pub mirror: bool,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self { version: VERSION, oldest: OLDEST, caps: Bits(CAP_DEFLATE_RAW), idle_ms: 15_000, window: 1 << 20, auto_credit: true, build: String::new(), mirror: true }
    }
}

/// What the page hears.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientEvent {
    Opened { version: u32, caps: Bits, born: u64 },
    Refused { code: OpenRefusal, why: String, build: String },
    Answer { id: u64, outcome: Outcome, at: Option<Mark>, secret: bool },
    /// The subscription's state starts over from a snapshot at `cursor`.
    Snapshot { sub: u32, cursor: Cursor },
    Resumed { sub: u32, cursor: Cursor },
    /// A change, its keys resolved (a `Tail` made whole).
    Change { sub: u32, rev: u64, delta: Delta },
    Ready { sub: u32, cursor: Cursor },
    End { sub: u32, code: EndCode, why: String },
    /// A change didn't follow from the cursor: subscribed again from it.
    Gap { sub: u32, cursor: Option<Cursor> },
    /// A frame the server sent couldn't be read (the server is told).
    Unreadable { kind: Option<u32>, why: Why },
    /// The server couldn't read one of this side's.
    Unread { kind: u32, code: UnreadCode, why: UnreadWhy },
    Closed { code: CloseCode, why: String, build: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Stream,
    Field(ItemKey, u32),
}

#[derive(Debug, Clone)]
struct ClientSub {
    topic: Topic,
    /// The cursor the page has (set at each `Ready`, moved by each change after).
    cursor: Option<Cursor>,
    /// The revision through which this subscription has everything so far.
    rev: u64,
    born: u64,
    /// Between `Snapshot`/`Resumed` and `Ready`.
    starting: bool,
    /// Waiting for a `Snapshot` or `Resumed` (after a gap): what comes before is dropped.
    waiting: bool,
    aliases: Vec<String>,
    tail: Option<(Target, u64)>,
    credit: u64,
    received: u64,
    state: TopicState,
    ended: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    Opening,
    Open,
    Closed,
}

/// The client half of a browser's connection.
#[derive(Debug)]
pub struct Client {
    cfg: ClientConfig,
    phase: Phase,
    version: u32,
    caps: Bits,
    born: u64,
    opened_at: u64,
    live: Liveness,
    events: VecDeque<ClientEvent>,
    up: VecDeque<(Frame, Prio)>,
    credits: BTreeMap<u32, u64>,
    resizes: BTreeMap<u32, (u16, u16)>,
    probe_want: bool,
    probe_out: Option<(u32, u64)>,
    acks: VecDeque<ProbeAck>,
    nonce: u32,
    subs: BTreeMap<u32, ClientSub>,
    /// Requests not yet answered finally, by id: sent again on the next connection.
    pending: BTreeMap<u64, Call>,
    joiner: Joiner,
    inflate: Inflaters,
    fragment: usize,
    got: u64,
    granted: u64,
    last_sent: u64,
    idle_due: bool,
    /// Frames read, by kind index (what a lab counts).
    pub counts: BTreeMap<u32, u64>,
}

impl Client {
    /// A connection opening at `now` with `ticket`.
    pub fn new(cfg: ClientConfig, ticket: &[u8], now: u64) -> Self {
        let open = Frame::Open(Open {
            version: cfg.version,
            oldest: cfg.oldest,
            caps: cfg.caps,
            schema: Bits(schema_hash()),
            ticket: Bytes(ticket.to_vec()),
            idle_ms: cfg.idle_ms,
            window: cfg.window,
            build: cfg.build.clone(),
        });
        let mut up = VecDeque::new();
        up.push_back((open, Prio::Control));
        let granted = cfg.window;
        Self {
            live: Liveness::new(u64::from(cfg.idle_ms), now),
            cfg,
            phase: Phase::Opening,
            version: OLDEST,
            caps: Bits(0),
            born: 0,
            opened_at: now,
            events: VecDeque::new(),
            up,
            credits: BTreeMap::new(),
            resizes: BTreeMap::new(),
            probe_want: false,
            probe_out: None,
            acks: VecDeque::new(),
            nonce: 0,
            subs: BTreeMap::new(),
            pending: BTreeMap::new(),
            joiner: Joiner::new(),
            inflate: Inflaters::new(MAX_FRAME.saturating_mul(4)),
            fragment: 1200,
            got: 0,
            granted,
            last_sent: now,
            idle_due: false,
            counts: BTreeMap::new(),
        }
    }

    /// A new connection carrying this one's subscriptions (from their cursors) and its requests
    /// not yet answered finally (under the same ids).
    pub fn reconnect(&self, cfg: ClientConfig, ticket: &[u8], now: u64) -> Self {
        let mut c = Self::new(cfg, ticket, now);
        for (id, call) in &self.pending {
            c.request(*id, call.clone());
        }
        for (id, s) in &self.subs {
            if !s.ended {
                c.subscribe_from(*id, s.topic.clone(), s.cursor.clone(), s.credit, s.state.clone());
            }
        }
        c
    }

    pub fn poll_event(&mut self) -> Option<ClientEvent> {
        self.events.pop_front()
    }

    pub fn is_open(&self) -> bool {
        self.phase == Phase::Open
    }

    pub fn is_closed(&self) -> bool {
        self.phase == Phase::Closed
    }

    pub fn version(&self) -> u32 {
        self.version
    }

    pub fn liveness(&self) -> &Liveness {
        &self.live
    }

    /// A subscription's state, as mirrored.
    pub fn state(&self, sub: u32) -> Option<&TopicState> {
        self.subs.get(&sub).map(|s| &s.state)
    }

    /// A subscription's cursor.
    pub fn cursor(&self, sub: u32) -> Option<Cursor> {
        self.subs.get(&sub).and_then(|s| s.cursor.clone())
    }

    pub fn pending(&self) -> &BTreeMap<u64, Call> {
        &self.pending
    }

    fn queue(&mut self, f: Frame, prio: Prio) {
        self.up.push_back((f, prio));
    }

    /// Sends a request under `id` (an id the browser makes, unique to it); kept until a final
    /// answer, and sent again on a reconnect.
    pub fn request(&mut self, id: u64, call: Call) {
        self.pending.insert(id, call.clone());
        self.queue(Frame::Request(Request { id, call }), Prio::Control);
    }

    pub fn subscribe(&mut self, sub: u32, topic: Topic, credit: u64) {
        self.subscribe_from(sub, topic, None, credit, TopicState::default());
    }

    fn subscribe_from(&mut self, sub: u32, topic: Topic, cursor: Option<Cursor>, credit: u64, state: TopicState) {
        let s = ClientSub { topic: topic.clone(), cursor: cursor.clone(), rev: cursor.as_ref().map_or(0, |c| c.rev), born: cursor.as_ref().map_or(0, |c| c.born), starting: false, waiting: true, aliases: Vec::new(), tail: None, credit, received: 0, state, ended: false };
        self.subs.insert(sub, s);
        self.queue(Frame::Subscribe(Subscribe { sub, topic, cursor, credit }), Prio::Control);
    }

    pub fn unsubscribe(&mut self, sub: u32) {
        if let Some(s) = self.subs.get_mut(&sub) {
            s.ended = true;
        }
        self.queue(Frame::Unsubscribe(Unsubscribe { sub }), Prio::Control);
    }

    /// Grants a subscription credit up to `upto` bytes since it began (0: the connection's).
    pub fn grant(&mut self, sub: u32, upto: u64) {
        if sub == 0 {
            self.granted = self.granted.max(upto);
        } else if let Some(s) = self.subs.get_mut(&sub) {
            s.credit = s.credit.max(upto);
        }
        let e = self.credits.entry(sub).or_insert(0);
        *e = (*e).max(upto);
    }

    /// Bytes a subscription has had.
    pub fn received(&self, sub: u32) -> u64 {
        self.subs.get(&sub).map_or(0, |s| s.received)
    }

    pub fn input(&mut self, sub: u32, data: &[u8]) {
        self.queue(Frame::Input(Input { sub, data: Bytes(data.to_vec()) }), Prio::Interactive);
    }

    pub fn resize(&mut self, sub: u32, cols: u16, rows: u16) {
        self.resizes.insert(sub, (cols, rows));
    }

    pub fn close(&mut self, code: CloseCode, why: &str) {
        if self.phase == Phase::Open {
            self.queue(Frame::Close(Close { code, why: why.into(), build: String::new() }), Prio::Control);
        }
        self.phase = Phase::Closed;
    }

    /// The bytes that came, at `now`.
    pub fn receive(&mut self, bytes: &[u8], now: u64) {
        if bytes.is_empty() {
            return;
        }
        self.got = self.got.saturating_add(bytes.len() as u64);
        self.live.arrived(now);
        let mut frames = Frames::new(bytes);
        while let Some(raw) = frames.next_raw() {
            let n = match &raw {
                Ok((_, b)) => b.len(),
                Err(_) => 0,
            };
            match raw.and_then(|(kind, body)| decode_body(kind, body)) {
                Ok(r) => self.take(r.frame, n, now, 0),
                Err(e) => self.refuse(e.kind, e.why),
            }
        }
        if self.cfg.auto_credit && self.phase == Phase::Open && self.got.saturating_mul(2) >= self.granted {
            let upto = self.got.saturating_add(self.cfg.window);
            self.grant(0, upto);
        }
    }

    fn refuse(&mut self, kind: Option<u32>, why: Why) {
        self.events.push_back(ClientEvent::Unreadable { kind, why });
        let code = match why.code() {
            crate::wire::Code::Unknown => UnreadCode::Unknown,
            crate::wire::Code::Malformed => UnreadCode::Malformed,
        };
        self.queue(Frame::Unread(Unread { kind: kind.unwrap_or(u32::MAX), code, why: UnreadWhy::of(why) }), Prio::Control);
    }

    fn take(&mut self, f: Frame, len: usize, now: u64, depth: u8) {
        *self.counts.entry(f.kind().index()).or_insert(0) += 1;
        match f {
            Frame::Accepted(a) => {
                self.phase = Phase::Open;
                self.version = a.version;
                self.caps = a.caps;
                self.born = a.born;
                self.fragment = usize::try_from(a.fragment).unwrap_or(1200).max(MIN_FRAGMENT);
                self.live.set_idle(u64::from(a.idle_ms));
                self.live.round_trip(now.saturating_sub(self.opened_at));
                self.events.push_back(ClientEvent::Opened { version: a.version, caps: a.caps, born: a.born });
            }
            Frame::Refused(r) => {
                self.phase = Phase::Closed;
                self.events.push_back(ClientEvent::Refused { code: r.code, why: r.why, build: r.build });
            }
            Frame::Answer(a) => self.answer(a, false),
            Frame::SecretAnswer(a) => self.answer(a, true),
            Frame::Snapshot(b) => self.begin(b, true),
            Frame::Resumed(b) => self.begin(b, false),
            Frame::Change(c) => {
                self.count(c.sub, len);
                self.change(c.sub, c.rev, c.step, c.delta);
            }
            Frame::Tail(t) => {
                self.count(t.sub, len);
                self.tail(t);
            }
            Frame::Ready(b) => {
                if let Some(s) = self.subs.get_mut(&b.sub) {
                    if s.waiting || s.ended {
                        return;
                    }
                    s.starting = false;
                    s.rev = b.cursor.rev;
                    s.born = b.cursor.born;
                    s.cursor = Some(b.cursor.clone());
                    self.events.push_back(ClientEvent::Ready { sub: b.sub, cursor: b.cursor });
                }
            }
            Frame::End(e) => {
                if let Some(s) = self.subs.get_mut(&e.sub) {
                    s.ended = true;
                }
                self.events.push_back(ClientEvent::End { sub: e.sub, code: e.code, why: e.why });
            }
            Frame::Fragment(fr) => match self.joiner.take(fr) {
                None => {}
                Some(Ok(r)) => self.take(r.frame, len, now, depth),
                Some(Err(e)) => self.refuse(e.kind, e.why),
            },
            Frame::Idle(_) => {}
            Frame::Probe(p) => self.acks.push_back(ProbeAck { nonce: p.nonce, got: self.got }),
            Frame::ProbeAck(a) => {
                if let Some((n, at)) = self.probe_out
                    && n == a.nonce
                {
                    self.live.round_trip(now.saturating_sub(at));
                    self.probe_out = None;
                }
            }
            Frame::Unread(u) => self.events.push_back(ClientEvent::Unread { kind: u.kind, code: u.code, why: u.why }),
            Frame::Compressed(c) => {
                if depth > 0 {
                    return self.refuse(Some(Kind::Compressed.index()), Why::TooDeep);
                }
                match self.inflate.inflate(c.ctx, None, &c.data.0) {
                    Ok(raw) if raw.len() != usize::try_from(c.len).unwrap_or(usize::MAX) => self.refuse(Some(Kind::Compressed.index()), Why::BadValue),
                    Ok(raw) => {
                        for r in Frames::new(&raw) {
                            match r {
                                Ok(r) if matches!(r.frame.kind(), Kind::Compressed | Kind::Fragment) => self.refuse(Some(r.frame.kind().index()), Why::TooDeep),
                                Ok(r) => self.take(r.frame, len, now, depth.saturating_add(1)),
                                Err(e) => self.refuse(e.kind, e.why),
                            }
                        }
                    }
                    Err(_) => self.refuse(Some(Kind::Compressed.index()), Why::BadValue),
                }
            }
            Frame::Close(c) => {
                self.phase = Phase::Closed;
                self.events.push_back(ClientEvent::Closed { code: c.code, why: c.why, build: c.build });
            }
            Frame::Open(_) | Frame::Request(_) | Frame::Subscribe(_) | Frame::Unsubscribe(_) | Frame::Credit(_) | Frame::Input(_) | Frame::Resize(_) => {
                let k = f.kind().index();
                self.refuse(Some(k), Why::OutOfOrder);
            }
        }
    }

    fn count(&mut self, sub: u32, len: usize) {
        if let Some(s) = self.subs.get_mut(&sub) {
            s.received = s.received.saturating_add(len as u64);
        }
    }

    fn answer(&mut self, a: Answer, secret: bool) {
        if a.outcome.finality() == Finality::Final {
            self.pending.remove(&a.id);
        }
        self.events.push_back(ClientEvent::Answer { id: a.id, outcome: a.outcome, at: a.at, secret });
    }

    fn begin(&mut self, b: Begin, snapshot: bool) {
        let Some(s) = self.subs.get_mut(&b.sub) else { return };
        if s.ended {
            return;
        }
        if !snapshot {
            // A resume must start from where this side is.
            let here = s.cursor.as_ref().map(|c| (c.born, c.rev));
            if here != Some((b.cursor.born, b.cursor.rev)) {
                return self.gap(b.sub);
            }
        }
        s.waiting = false;
        s.starting = true;
        s.aliases.clear();
        s.tail = None;
        s.rev = b.cursor.rev;
        s.born = b.cursor.born;
        if snapshot {
            s.state = TopicState::default();
            self.events.push_back(ClientEvent::Snapshot { sub: b.sub, cursor: b.cursor });
        } else {
            self.events.push_back(ClientEvent::Resumed { sub: b.sub, cursor: b.cursor });
        }
    }

    /// Subscribes again from the cursor the page has.
    fn gap(&mut self, sub: u32) {
        let Some(s) = self.subs.get_mut(&sub) else { return };
        s.waiting = true;
        let cursor = s.cursor.clone();
        let (topic, credit) = (s.topic.clone(), s.credit);
        s.received = 0;
        self.events.push_back(ClientEvent::Gap { sub, cursor: cursor.clone() });
        self.queue(Frame::Subscribe(Subscribe { sub, topic, cursor, credit }), Prio::Control);
    }

    fn resolve(s: &mut ClientSub, k: &Key) -> Option<Key> {
        match k {
            Key::Ref { n } => s.aliases.get(usize::try_from(*n).ok()?).map(|name| Key::Str { s: name.clone() }),
            Key::Str { s: name } => {
                if s.aliases.len() < MAX_ALIASES {
                    s.aliases.push(name.clone());
                }
                Some(k.clone())
            }
            Key::Num { .. } => Some(k.clone()),
        }
    }

    fn change(&mut self, sub: u32, rev: u64, step: u64, delta: Delta) {
        let mirror = self.cfg.mirror;
        let Some(s) = self.subs.get_mut(&sub) else { return };
        if s.waiting || s.ended {
            return;
        }
        let snapshot_item = step == 0;
        if !snapshot_item {
            if rev <= s.rev {
                return; // a repeat
            }
            if rev.saturating_sub(step) != s.rev {
                return self.gap(sub);
            }
        } else if !s.starting {
            return self.gap(sub);
        }
        let resolved = match &delta {
            Delta::Put { key, fields } => Self::resolve(s, key).map(|key| Delta::Put { key, fields: fields.clone() }),
            Delta::Patch { key, mask, fields } => Self::resolve(s, key).map(|key| Delta::Patch { key, mask: *mask, fields: fields.clone() }),
            Delta::Remove { key } => Self::resolve(s, key).map(|key| Delta::Remove { key }),
            Delta::Extend { key, field, at, data } => Self::resolve(s, key).map(|key| Delta::Extend { key, field: *field, at: *at, data: data.clone() }),
            Delta::Set { .. } | Delta::Append { .. } => Some(delta.clone()),
        };
        let Some(d) = resolved else { return self.gap(sub) };
        s.tail = match &d {
            Delta::Append { at, data } => Some((Target::Stream, at.saturating_add(data.0.len() as u64))),
            Delta::Extend { key, field, at, data } => ItemKey::of(key).map(|k| (Target::Field(k, *field), at.saturating_add(data.0.len() as u64))),
            _ => None,
        };
        if mirror && s.state.apply(&d).is_err() {
            return self.gap(sub);
        }
        if !snapshot_item {
            s.rev = rev;
            if !s.starting {
                s.cursor = Some(Cursor { born: s.born, rev });
            }
        }
        self.events.push_back(ClientEvent::Change { sub, rev, delta: d });
    }

    fn tail(&mut self, t: Tail) {
        let mirror = self.cfg.mirror;
        let Some(s) = self.subs.get_mut(&t.sub) else { return };
        if s.waiting || s.ended {
            return;
        }
        let Some((target, end)) = s.tail.clone() else { return self.gap(t.sub) };
        let len = t.data.0.len() as u64;
        let d = match &target {
            Target::Stream => Delta::Append { at: end, data: t.data },
            Target::Field(k, f) => Delta::Extend { key: k.key(), field: *f, at: end, data: t.data },
        };
        let applied: Result<(), Misfit> = if mirror { s.state.apply(&d) } else { Ok(()) };
        if applied.is_err() {
            return self.gap(t.sub);
        }
        s.tail = Some((target, end.saturating_add(len)));
        s.rev = s.rev.saturating_add(1);
        if !s.starting {
            s.cursor = Some(Cursor { born: s.born, rev: s.rev });
        }
        let rev = s.rev;
        self.events.push_back(ClientEvent::Change { sub: t.sub, rev, delta: d });
    }

    /// The page ran again after a freeze: the gap is its own, not the link's.
    pub fn resumed(&mut self, now: u64) {
        self.live.resumed(now);
        self.probe_out = None;
    }

    /// The link's verdict now; a suspect link gets a probe.
    pub fn verdict(&mut self, now: u64) -> Verdict {
        let v = self.live.verdict(now);
        if v == Verdict::Suspect && self.phase == Phase::Open && !(self.probe_out.is_some() && self.live.probing()) {
            self.probe_want = true;
        }
        v
    }

    pub fn poll_timeout(&self, now: u64) -> u64 {
        self.last_sent.saturating_add(u64::from(self.cfg.idle_ms)).min(self.live.next_change(now))
    }

    /// Bytes to send, at most `room`.
    pub fn poll_transmit(&mut self, room: usize, now: u64) -> Option<Transmit> {
        if self.phase == Phase::Open {
            let _ = self.verdict(now);
            for (sub, upto) in std::mem::take(&mut self.credits) {
                self.queue(Frame::Credit(Credit { sub, upto }), Prio::Control);
            }
            for (sub, (cols, rows)) in std::mem::take(&mut self.resizes) {
                self.queue(Frame::Resize(Resize { sub, cols, rows }), Prio::Interactive);
            }
            if self.up.is_empty() && now.saturating_sub(self.last_sent) >= u64::from(self.cfg.idle_ms) {
                self.idle_due = true;
            }
        }
        let mut t = Transmit::default();
        let push = |t: &mut Transmit, kind: Kind, lane: Lane, prio: Prio, b: &[u8]| {
            t.frames.push(Sent { kind, lane, prio, urgent: crate::conn::info(kind).urgent == "yes", start: t.bytes.len(), len: b.len() });
            t.bytes.extend_from_slice(b);
        };
        if self.probe_want && self.phase == Phase::Open {
            self.nonce = self.nonce.wrapping_add(1);
            let b = Frame::Probe(Probe { nonce: self.nonce }).encode(self.version);
            self.probe_want = false;
            self.probe_out = Some((self.nonce, now));
            self.live.probe_sent(now);
            push(&mut t, Kind::Probe, Lane::Datagram, Prio::Control, &b);
        }
        while let Some(a) = self.acks.pop_front() {
            let b = Frame::ProbeAck(a).encode(self.version);
            push(&mut t, Kind::ProbeAck, Lane::Datagram, Prio::Control, &b);
        }
        // Keystrokes and sizes ahead of the rest.
        let mut order: Vec<usize> = (0..self.up.len()).filter(|i| self.up.get(*i).is_some_and(|u| u.1 == Prio::Interactive)).collect();
        order.extend((0..self.up.len()).filter(|i| self.up.get(*i).is_some_and(|u| u.1 != Prio::Interactive)));
        let mut taken = Vec::new();
        let version = if self.phase == Phase::Opening { self.cfg.version } else { self.version };
        for i in order {
            let Some((f, prio)) = self.up.get(i) else { continue };
            let kind = f.kind();
            if self.phase == Phase::Opening && kind != Kind::Open {
                continue;
            }
            let b = f.encode(version);
            if b.len() > self.fragment && kind != Kind::Open {
                taken.push(i);
                let prio = *prio;
                for fr in cut(0, &b, self.fragment) {
                    let fb = fr.encode(version);
                    push(&mut t, Kind::Fragment, Lane::Control, prio, &fb);
                }
                continue;
            }
            if t.bytes.len().saturating_add(b.len()) > room && !t.bytes.is_empty() {
                break;
            }
            let lane = if kind == Kind::Idle { Lane::Datagram } else { Lane::Control };
            push(&mut t, kind, lane, *prio, &b);
            taken.push(i);
        }
        taken.sort_unstable();
        for i in taken.into_iter().rev() {
            self.up.remove(i);
        }
        if t.bytes.is_empty() && self.idle_due && self.phase == Phase::Open {
            let b = Frame::Idle(Idle { got: self.got }).encode(self.version);
            push(&mut t, Kind::Idle, Lane::Datagram, Prio::Control, &b);
        }
        self.idle_due = false;
        if t.bytes.is_empty() {
            return None;
        }
        self.last_sent = now;
        Some(t)
    }
}

/// Reads one frame a fragment ended with (for tools that join by hand).
pub fn joined(bytes: &[u8]) -> Result<Read, Fault> {
    read_one(bytes, MAX_FRAME)
}

/// A field's text length (for tools that follow an `Extend`).
pub fn field_text_len(f: &Bytes) -> u64 {
    text_len(f)
}
