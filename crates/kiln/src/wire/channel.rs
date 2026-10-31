//! The channel between a runner and the server (docs/wire.md): each message that must arrive is
//! an item of a stream, numbered there, kept by its sender until the receiver answers it (an ACK,
//! or a NACK saying why not), and sent again after a reconnect from where the receiver says it
//! has the stream. An item that comes twice is taken once.
//!
//! Sans-io: frames, ticks and reconnects in; frames and answers out. The clock is the caller's, as
//! is the link.
//!
//! - **Answers ride together.** One `Ack` says every item through `upto` is answered, and lists
//!   the ones of them it refused (the NACKs) with how and why. A refusal is never mistaken for a
//!   taking when a frame is lost: each item carries `low`, through which its sender has every
//!   answer, and the receiver repeats every refusal after it.
//! - **Gaps.** An item past a gap is dropped, and its receiver asks (`Want`) for what follows the
//!   gap once, and again only every so many items past it: its sender sends that again at once, and a want that comes after (made before the
//!   receiver had them again, one for each item that was past the gap) sends it again only once
//!   [`WANT_AGAIN`] has passed. A receiver that's behind its sender's `low` (it started again with
//!   less than that) goes on from there: every item through it is answered.
//! - **Reconnects.** When a link comes up, each side says through which item it has each stream
//!   it receives (`Have`), and its sender sends again only what follows. Nothing goes until then.
//!   Each side says it again every few seconds, so a `Have` lost below the channel doesn't hold a
//!   link's sends up: a sender that's up takes another as answers only.
//! - **Restarts.** A durable stream's sender keeps its unanswered items (and its receiver what it
//!   has answered) where they outlive the process (`crate::wire::keep`), and picks up from there.
//!   A memory stream's sender gets a new stream name with each start (its incarnation in the
//!   name), so a receiver never mistakes a new item for an old one; a receiver that says it has
//!   less than it had answered lost what it kept, and its sender is told (`Event::Regressed`).
//! - **Room.** A stream keeps at most so many bytes unanswered: past that its producer waits
//!   ([`Full`]), and nothing is shed. What a stream starts with, its picture of what it carries
//!   now ([`Sender::push_picture`]), is taken whatever its size: the room bounds what comes after.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// An item's number in its stream, from 1. 0: none yet.
pub type Seq = u64;

/// How a receiver refused an item: the kinds its sender can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    /// It wouldn't do it; `why` says why.
    Refused,
    /// What it names isn't there: a session that ended, an approval no longer waiting.
    Unknown,
    /// It's for an epoch the receiver has left: the session went on elsewhere.
    StaleEpoch,
    /// The receiver can't do that: an older build.
    Unsupported,
    /// It couldn't be read.
    Malformed,
}

/// A refused item: its number, how, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Nack {
    pub seq: Seq,
    pub code: Refusal,
    pub why: String,
}

/// Through which item a receiver has a stream, and the refusals among them its sender may not
/// have heard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Has {
    pub stream: String,
    pub upto: Seq,
    pub nacks: Vec<Nack>,
}

/// A frame of the channel, either way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Frame {
    /// Item `seq` of `stream`. `low`: its sender has every answer through it. `body`: the message
    /// it carries, encoded as its wire encodes it.
    Item { stream: String, seq: Seq, low: Seq, body: Vec<u8> },
    /// Every item of the stream through `upto` is answered; those of them refused are in `nacks`.
    Ack(Has),
    /// Send `stream` again from item `from`: the receiver saw a gap.
    Want { stream: String, from: Seq },
    /// A link came up: through which item this side has each stream it receives.
    Have(Vec<Has>),
}

/// What became of an item, for whoever sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Taken,
    Refused { code: Refusal, why: String },
}

/// What a sender learns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// An item was answered.
    Answered { stream: String, seq: Seq, outcome: Outcome },
    /// The receiver has `stream` only through `has`, short of what it had answered: it lost what
    /// it kept (a restart). What it lost can't come from here; the stream's owner resyncs it.
    Regressed { stream: String, has: Seq, answered: Seq },
}

/// A stream is full: its producer waits until answers make room.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Full;

/// How long after an item went again for a want another want for it is taken as made before the
/// receiver had it again (each item past a gap brings one): until then it doesn't send it again.
pub const WANT_AGAIN: Duration = Duration::from_millis(250);

/// Whether an item was wanted since it last went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wanted {
    No,
    /// Its first want since it went: it goes again at once.
    Now,
    /// A want after it went again for one: it goes again once [`WANT_AGAIN`] has passed since.
    Again,
}

struct Pending {
    seq: Seq,
    body: Vec<u8>,
    /// Part of its stream's picture ([`Sender::push_picture`]): not counted against the room.
    picture: bool,
    /// When it was last sent since the link came up; `None`: not yet.
    sent: Option<Instant>,
    wanted: Wanted,
    /// When it last went again for a want, on this link.
    went_for_want: Option<Instant>,
}

impl Pending {
    fn new(seq: Seq, body: Vec<u8>, picture: bool) -> Self {
        Self { seq, body, picture, sent: None, wanted: Wanted::No, went_for_want: None }
    }

    /// Not sent on this link yet: it goes as if new.
    fn unsent(&mut self) {
        self.sent = None;
        self.wanted = Wanted::No;
        self.went_for_want = None;
    }
}

struct Out {
    /// The number the next item gets.
    next: Seq,
    /// Every item through here is answered.
    answered: Seq,
    pending: VecDeque<Pending>,
    bytes: usize,
    /// Of `bytes`, its picture's: the room bounds only the rest, its backlog.
    picture: usize,
}

impl Default for Out {
    fn default() -> Self {
        Self { next: 1, answered: 0, pending: VecDeque::new(), bytes: 0, picture: 0 }
    }
}

impl Out {
    /// What it keeps unanswered past its picture.
    fn backlog(&self) -> usize {
        self.bytes.saturating_sub(self.picture)
    }

    /// The next item, its number.
    fn queue(&mut self, body: Vec<u8>, picture: bool) -> Seq {
        let seq = self.next;
        self.next = self.next.saturating_add(1);
        self.bytes = self.bytes.saturating_add(body.len());
        if picture {
            self.picture = self.picture.saturating_add(body.len());
        }
        self.pending.push_back(Pending::new(seq, body, picture));
        seq
    }
}

/// The sending side of the streams to one peer.
pub struct Sender {
    streams: BTreeMap<String, Out>,
    /// The most bytes a stream keeps unanswered past its picture (an item larger than that goes
    /// alone).
    room: usize,
    /// How long an item sent on a live link waits for its answer before it goes again: an answer
    /// lost below the channel (a frame shed) is made up for.
    patience: Duration,
    /// The link is up and the peer has said what it has.
    up: bool,
}

impl Sender {
    pub fn new(room: usize, patience: Duration) -> Self {
        Self { streams: BTreeMap::new(), room, patience, up: false }
    }

    /// U01: inspect the next unconsumed slot under the SAME Commands mutex. This publishes nothing.
    pub(crate) fn next_sequence(&self, stream: &str) -> Seq { self.streams.get(stream).map_or(1, |out| out.next) }

    /// Queues `body` as the next item of `stream`: its number, or [`Full`] while the stream
    /// keeps as much unanswered past its picture as it may.
    pub fn push(&mut self, stream: &str, body: Vec<u8>) -> Result<Seq, Full> {
        let o = self.streams.entry(stream.to_owned()).or_default();
        if o.backlog() > 0 && o.backlog().saturating_add(body.len()) > self.room {
            return Err(Full);
        }
        Ok(o.queue(body, false))
    }

    /// Queues `body` as the next item of `stream`'s picture, what it starts with (a session's
    /// snapshot, a terminal's replay): taken whatever its size, as a stream whose picture didn't
    /// fit its room could never start. The room bounds what's pushed after it.
    pub fn push_picture(&mut self, stream: &str, body: Vec<u8>) -> Seq {
        self.streams.entry(stream.to_owned()).or_default().queue(body, true)
    }

    /// Picks a durable stream up after a restart: the number its next item gets, and what it
    /// had sent and not had answered (`crate::wire::keep`), in order.
    pub fn restore(&mut self, stream: &str, next: Seq, pending: Vec<(Seq, Vec<u8>)>) {
        let o = self.streams.entry(stream.to_owned()).or_default();
        o.pending = pending.into_iter().map(|(seq, body)| Pending::new(seq, body, false)).collect();
        o.bytes = o.pending.iter().map(|p| p.body.len()).sum();
        o.picture = 0;
        o.answered = o.pending.front().map_or(next.saturating_sub(1), |p| p.seq.saturating_sub(1));
        o.next = next.max(o.pending.back().map_or(1, |p| p.seq.saturating_add(1)));
    }

    /// The frames to send now: items not yet sent since the link came up, and those whose answer
    /// is overdue. Nothing while the link is down or the peer hasn't said what it has.
    pub fn poll(&mut self, now: Instant) -> Vec<Frame> {
        let mut out = Vec::new();
        if !self.up {
            return out;
        }
        for (stream, o) in &mut self.streams {
            let low = o.answered;
            for p in &mut o.pending {
                let due = match p.sent {
                    None => true,
                    Some(at) if now.saturating_duration_since(at) >= self.patience => true,
                    Some(_) => match p.wanted {
                        Wanted::No => false,
                        Wanted::Now => true,
                        Wanted::Again => p.went_for_want.is_none_or(|at| now.saturating_duration_since(at) >= WANT_AGAIN),
                    },
                };
                if due {
                    if p.wanted != Wanted::No {
                        p.went_for_want = Some(now);
                    }
                    p.wanted = Wanted::No;
                    p.sent = Some(now);
                    out.push(Frame::Item { stream: stream.clone(), seq: p.seq, low, body: p.body.clone() });
                }
            }
        }
        out
    }

    /// U05: a finite fair turn of this SAME sender. Only selected items get their
    /// sent timestamp, so a held first stream cannot starve another producer.
    pub(crate) fn poll_bounded(&mut self, now: Instant, limit: usize, after: &mut Option<(String,Seq)>) -> Vec<Frame> {
        if !self.up || limit == 0 {return Vec::new()}
        let mut keys=self.streams.iter().flat_map(|(stream,out)|out.pending.iter().map(move |pending|(stream.clone(),pending.seq))).collect::<Vec<_>>();
        if let Some(previous)=after.as_ref(){keys.sort_by_key(|key|key<=previous);}
        let mut frames=Vec::new();
        for (stream,seq) in keys {
            let Some(out)=self.streams.get_mut(&stream) else{continue};
            let Some(p)=out.pending.iter_mut().find(|pending|pending.seq==seq) else{continue};
            let due=match p.sent {
                None=>true,
                Some(at) if now.saturating_duration_since(at)>=self.patience=>true,
                Some(_)=>match p.wanted{Wanted::No=>false,Wanted::Now=>true,Wanted::Again=>p.went_for_want.is_none_or(|at|now.saturating_duration_since(at)>=WANT_AGAIN)},
            };
            if !due{continue}
            if p.wanted!=Wanted::No{p.went_for_want=Some(now);}
            p.wanted=Wanted::No;p.sent=Some(now);
            *after=Some((stream.clone(),seq));
            frames.push(Frame::Item{stream,seq,low:out.answered,body:p.body.clone()});
            if frames.len()==limit{break}
        }
        frames
    }

    /// The link went down: what was sent on it goes again once the peer says what it has.
    pub fn down(&mut self) {
        self.up = false;
        for o in self.streams.values_mut() {
            for p in &mut o.pending {
                p.unsent();
            }
        }
    }

    /// A frame from the peer for this side: its answers, its wants, what it has after a reconnect.
    pub fn on_frame(&mut self, frame: Frame) -> Vec<Event> {
        match frame {
            Frame::Ack(has) => self.answered(has, false),
            Frame::Have(all) => {
                let mut events = Vec::new();
                let mut said: BTreeMap<String, Has> = all.into_iter().map(|h| (h.stream.clone(), h)).collect();
                let streams: Vec<String> = self.streams.keys().cloned().collect();
                // Only a link's first says the receiver lost what it had (it started again): one
                // said again may have been made before answers that reached here ahead of it.
                let first = !self.up;
                for stream in streams {
                    let has = said.remove(&stream).unwrap_or(Has { stream, upto: 0, nacks: Vec::new() });
                    events.extend(self.answered(has, first));
                }
                // The link's first: what wasn't answered goes again. Another on the same link (said
                // again, as each side does every few seconds) is answers only.
                if !self.up {
                    for o in self.streams.values_mut() {
                        for p in &mut o.pending {
                            p.unsent();
                        }
                    }
                }
                self.up = true;
                events
            }
            Frame::Want { stream, from } => {
                if let Some(o) = self.streams.get_mut(&stream) {
                    for p in o.pending.iter_mut().filter(|p| p.seq >= from) {
                        p.wanted = if p.went_for_want.is_none() { Wanted::Now } else { Wanted::Again };
                    }
                }
                Vec::new()
            }
            Frame::Item { .. } => Vec::new(),
        }
    }

    fn answered(&mut self, has: Has, reconnect: bool) -> Vec<Event> {
        let mut events = Vec::new();
        let Some(o) = self.streams.get_mut(&has.stream) else { return events };
        if reconnect && has.upto < o.answered {
            events.push(Event::Regressed { stream: has.stream.clone(), has: has.upto, answered: o.answered });
        }
        while let Some(p) = o.pending.front() {
            if p.seq > has.upto {
                break;
            }
            let seq = p.seq;
            let outcome = match has.nacks.iter().find(|n| n.seq == seq) {
                Some(n) => Outcome::Refused { code: n.code, why: n.why.clone() },
                None => Outcome::Taken,
            };
            if let Some(p) = o.pending.pop_front() {
                o.bytes = o.bytes.saturating_sub(p.body.len());
                if p.picture {
                    o.picture = o.picture.saturating_sub(p.body.len());
                }
            }
            events.push(Event::Answered { stream: has.stream.clone(), seq, outcome });
        }
        o.answered = o.answered.max(has.upto.min(o.next.saturating_sub(1)));
        events
    }

    /// Lets go of a stream: it ended, or starts over under another name. What it kept
    /// unanswered (by number) is dropped with it, never sent.
    pub fn drop_stream(&mut self, stream: &str) -> Vec<Seq> {
        self.streams.remove(stream).map(|o| o.pending.into_iter().map(|p| p.seq).collect()).unwrap_or_default()
    }

    /// The streams this side sends.
    pub fn streams(&self) -> impl Iterator<Item = &str> {
        self.streams.keys().map(String::as_str)
    }

    /// What `stream` keeps unanswered, oldest first (what a durable stream keeps on disk).
    pub fn pending(&self, stream: &str) -> Vec<Seq> {
        self.streams.get(stream).map(|o| o.pending.iter().map(|p| p.seq).collect()).unwrap_or_default()
    }

    /// Every stream's unanswered items, counted.
    pub fn unanswered(&self) -> usize {
        self.streams.values().map(|o| o.pending.len()).sum()
    }

    /// The bytes every stream keeps unanswered, together.
    pub fn bytes(&self) -> usize {
        self.streams.values().map(|o| o.bytes).sum()
    }

    /// The bytes every stream keeps unanswered past its picture, together.
    pub fn backlog(&self) -> usize {
        self.streams.values().map(Out::backlog).sum()
    }
}

/// Refusals a receiver keeps for a stream whose sender hasn't heard them, at the most.
const NACKS_KEPT: usize = 4096;

#[derive(Default)]
struct In {
    /// Every item through here is answered.
    upto: Seq,
    /// The item being applied now: a copy that comes meanwhile is dropped (its answer comes once
    /// it's applied).
    taking: Option<Seq>,
    /// Refusals after the sender's `low`: repeated in every answer until it has them.
    nacks: BTreeMap<Seq, (Refusal, String)>,
    /// What it last asked for (`Want`), until it has it, and how many items past that gap came
    /// since: asked again once every [`WANT_EVERY`] of them (a want lost below the channel), not
    /// for each.
    wanted: Option<(Seq, u32)>,
}

/// Items past one gap a receiver takes in before it asks for the gap again.
const WANT_EVERY: u32 = 64;

impl In {
    fn has(&self, stream: &str) -> Has {
        Has {
            stream: stream.to_owned(),
            upto: self.upto,
            nacks: self.nacks.iter().map(|(seq, (code, why))| Nack { seq: *seq, code: *code, why: why.clone() }).collect(),
        }
    }
}

/// What to do with an item that came.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arrival {
    /// Apply it, then say [`Receiver::took`] or [`Receiver::refused`].
    Take(Vec<u8>),
    /// It was answered already: send this answer again. `None`: it's being applied, and its
    /// answer comes once it is.
    Answered(Option<Frame>),
    /// It's past a gap: send this `Want` back. The item is dropped, and comes again in order.
    /// (Once for a gap: the items after it that come before the gap's are dropped answered
    /// `Answered(None)`.)
    Gap(Frame),
}

// U01/U03: SAME Receiver owns bounded raw input and the distinct durable ACK frontier.
const ORDINARY_PENDING: usize = 64;
const ORDINARY_WINDOW: Seq = 4096;
// Reserve the disposition BEFORE any effect, even when raw input fills ROOM. The
// diagnostic may be shortened explicitly; its refusal code is never discarded.
const ORDINARY_OUTCOME_ROOM: usize = 8192;
const ORDINARY_WHY_MAX: usize = 1024;
// Normal raw input cannot consume these SAME-owner emergency slots/bytes. Both
// full-size controls fit; the finite reserve does not authorize effects past raw gaps.
const ORDINARY_URGENT_PENDING: usize = 2;
const ORDINARY_URGENT_ROOM: usize = ORDINARY_URGENT_PENDING*(super::commands::ITEM_MOST+ORDINARY_OUTCOME_ROOM);
fn ordinary_reason(mut why: String) -> String {
    const MARK: &str = " [truncated]";
    if why.len()>ORDINARY_WHY_MAX {
        let mut end=ORDINARY_WHY_MAX-MARK.len();
        while !why.is_char_boundary(end) {end-=1;}
        why.truncate(end);why.push_str(MARK);
    }
    why
}
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum OrdinaryStageRefusal {
    #[error("ordinary receiver needs its exact initialized origin/domain")] Domain,
    #[error("ordinary receiver pending input/outcome budget is full")] Full,
    #[error("ordinary command changed bytes or its already-owned disposition")] Conflict,
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OrdinaryArrival { Queued, Duplicate, Answered(Frame), Gap(Frame), AdmittedPending }
#[derive(Clone)]
pub(crate) struct OrdinaryTicket {
    origin: crate::net::Connection, nonce: String, stream: String, seq: Seq,
    body: std::sync::Arc<[u8]>, hash: String, authority: std::sync::Arc<()>,
}
impl OrdinaryTicket {
    pub(crate) fn origin(&self) -> crate::net::Connection { self.origin }
    pub(crate) fn nonce(&self) -> &str { &self.nonce }
    pub(crate) fn stream(&self) -> &str { &self.stream }
    pub(crate) fn seq(&self) -> Seq { self.seq }
    pub(crate) fn body(&self) -> &[u8] { &self.body }
    pub(crate) fn body_hash(&self) -> &str { &self.hash }
    pub(crate) fn same_identity(&self,other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.authority,&other.authority) && self.origin==other.origin && self.nonce==other.nonce && self.stream==other.stream && self.seq==other.seq && self.hash==other.hash
    }
}
#[derive(Clone)]
pub(crate) struct OrdinaryClosure {
    origin: crate::net::Connection, nonce: String, has: Has, low: Seq,
    base: Seq, tickets: Vec<(Seq,String)>, authority: std::sync::Arc<()>,
}
impl OrdinaryClosure {
    pub(crate) fn origin(&self) -> crate::net::Connection { self.origin }
    pub(crate) fn nonce(&self) -> &str { &self.nonce }
    pub(crate) fn has(&self) -> &Has { &self.has }
    pub(crate) fn low(&self) -> Seq { self.low }
}
enum OrdinaryEffect { Queued, Claimed, AdmittedPending, Done(Option<(Refusal,String)>) }
struct OrdinaryItem { ticket: OrdinaryTicket, effect: OrdinaryEffect, urgent: bool }
struct OrdinaryIn {
    origin: crate::net::Connection, nonce: String, stream: String, admitted: Seq,
    low: Seq, bytes: usize, pending: BTreeMap<Seq,OrdinaryItem>, authority: std::sync::Arc<()>,
}
pub(crate) fn ordinary_has_valid(has: &Has) -> Result<(),OrdinaryStageRefusal> {
    let name=has.stream.strip_prefix("l:").and_then(|s|s.split_once('.'));
    if !name.is_some_and(|(b,g)|b.parse::<u64>().is_ok() && g.parse::<u64>().ok().is_some_and(|g|g>0 && g<u64::MAX)) || has.upto==u64::MAX || has.nacks.len()>NACKS_KEPT { return Err(OrdinaryStageRefusal::Domain); }
    let mut last=0;
    for nack in &has.nacks {
        if nack.seq<=last || nack.seq>has.upto {return Err(OrdinaryStageRefusal::Conflict);}
        last=nack.seq;
    }
    if serde_json::to_vec(has).map_err(|_|OrdinaryStageRefusal::Conflict)?.len()>kiln_seams::records::VALUE_MAX-4096 {return Err(OrdinaryStageRefusal::Full);}
    Ok(())
}

/// The receiving side of the streams from one peer. Strict ownership stays in this owner;
/// legacy upto/nacks remain the ONLY values which Have/ACK may expose.
#[derive(Default)]
pub struct Receiver {
    streams: BTreeMap<String, In>,
    ordinary: Option<OrdinaryIn>,
}

impl Receiver {
    pub fn new() -> Self {
        Self::default()
    }

    /// An item came.
    pub fn on_item(&mut self, stream: &str, seq: Seq, low: Seq, body: Vec<u8>) -> Arrival { self.on_item_inner(stream, seq, low, body, true) }
    /// U01/U03: a sender low watermark cannot replace this receiver's durable admission.
    pub(crate) fn on_item_strict(&mut self, stream: &str, seq: Seq, low: Seq, body: Vec<u8>) -> Arrival { self.on_item_inner(stream, seq, low, body, false) }
    fn on_item_inner(&mut self, stream: &str, seq: Seq, low: Seq, body: Vec<u8>, allow_low: bool) -> Arrival {
        if self.ordinary.as_ref().is_some_and(|o|o.stream==stream) { return Arrival::Answered(None); }
        let i = self.streams.entry(stream.to_owned()).or_default();
        i.nacks.retain(|n, _| *n > low);
        if i.taking == Some(seq) {
            return Arrival::Answered(None);
        }
        // Its sender has every answer through `low`: what this side doesn't know of them (it
        // started again since, and the stream went on) isn't missing.
        if allow_low && i.taking.is_none() && i.upto < low {
            i.upto = low;
        }
        if seq <= i.upto {
            return Arrival::Answered(Some(Frame::Ack(i.has(stream))));
        }
        if seq == i.upto.saturating_add(1) && i.taking.is_none() {
            i.taking = Some(seq);
            i.wanted = None;
            return Arrival::Take(body);
        }
        let from = i.upto.saturating_add(1);
        match &mut i.wanted {
            Some((asked, past)) if *asked == from => {
                *past += 1;
                if *past % WANT_EVERY != 0 {
                    return Arrival::Answered(None);
                }
            }
            w => *w = Some((from, 0)),
        }
        Arrival::Gap(Frame::Want { stream: stream.to_owned(), from })
    }

    /// U03: application storage failed; leave the sequence unacknowledged and retryable.
    pub fn retry(&mut self, stream: &str, seq: Seq) {
        if let Some(state) = self.streams.get_mut(stream) && state.taking == Some(seq) {
            state.taking = None;
        }
    }

    /// The item being applied was taken: the answer to send.
    pub fn took(&mut self, stream: &str, seq: Seq) -> Frame {
        self.done(stream, seq, None)
    }

    /// The item being applied was refused: the answer to send.
    pub fn refused(&mut self, stream: &str, seq: Seq, code: Refusal, why: impl Into<String>) -> Frame {
        self.done(stream, seq, Some((code, why.into())))
    }

    fn done(&mut self, stream: &str, seq: Seq, refusal: Option<(Refusal, String)>) -> Frame {
        if self.ordinary.as_ref().is_some_and(|o|o.stream==stream) { return Frame::Ack(self.state(stream)); }
        let i = self.streams.entry(stream.to_owned()).or_default();
        if i.taking == Some(seq) {
            i.taking = None;
            i.upto = seq;
            if let Some(r) = refusal {
                i.nacks.insert(seq, r);
                while i.nacks.len() > NACKS_KEPT {
                    i.nacks.pop_first();
                }
            }
        }
        Frame::Ack(i.has(stream))
    }

    /// What this side has of every stream it receives: the first frame once a link comes up.
    pub fn have(&self) -> Frame {
        Frame::Have(self.streams.iter().map(|(s, i)| i.has(s)).collect())
    }

    /// The link went down: an item being applied is answered when it's done, on the next link.
    pub fn down(&mut self) {}

    /// What a durable stream's receiver keeps (`crate::wire::keep`): through which item it has
    /// it, and its refusals its sender may not have heard.
    pub fn state(&self, stream: &str) -> Has {
        self.streams.get(stream).map_or(Has { stream: stream.to_owned(), upto: 0, nacks: Vec::new() }, |i| i.has(stream))
    }

    /// Picks a durable stream up after a restart, from what it kept.
    pub fn restore(&mut self, has: Has) {
        if self.ordinary.as_ref().is_some_and(|o|o.stream==has.stream) { return; }
        let i = self.streams.entry(has.stream.clone()).or_default();
        i.upto = has.upto;
        i.taking = None;
        i.nacks = has.nacks.into_iter().map(|n| (n.seq, (n.code, n.why))).collect();
    }

    /// Lets go of a stream that ended (a session closed, a sender's earlier incarnation).
    pub fn forget(&mut self, stream: &str) {
        if self.ordinary.as_ref().is_some_and(|o|o.stream==stream) { return; }
        self.streams.remove(stream);
    }

    /// Lets go of every stream whose name `drop` picks.
    pub fn forget_where(&mut self, mut drop: impl FnMut(&str) -> bool) {
        let protected=self.ordinary.as_ref().map(|o|o.stream.clone());
        self.streams.retain(|s, _| protected.as_ref()==Some(s) || !drop(s));
    }
}

impl Receiver {
    /// Bind ONCE from the authoritative complete incoming state, before strict readiness.
    pub(crate) fn bind_ordinary(&mut self,origin: crate::net::Connection,nonce: &str,durable: Has,admitted: Seq) -> Result<(),OrdinaryStageRefusal> {
        ordinary_has_valid(&durable)?;
        if !crate::proto::valid_process_nonce(nonce) || admitted<durable.upto || admitted==u64::MAX || admitted-durable.upto>ORDINARY_WINDOW || self.ordinary.is_some() {return Err(OrdinaryStageRefusal::Domain);}
        if self.streams.get(&durable.stream).is_some_and(|i|i.taking.is_some() || (i.upto!=0 && i.has(&durable.stream)!=durable)) {return Err(OrdinaryStageRefusal::Conflict);}
        self.restore(durable.clone());
        self.ordinary=Some(OrdinaryIn {origin,nonce:nonce.into(),stream:durable.stream,admitted,low:0,bytes:0,pending:BTreeMap::new(),authority:std::sync::Arc::new(())});
        Ok(())
    }
    /// Refresh SAME authority after a retained positive Gate closes admission. That
    /// closure never fabricates a completed disposition or re-dispatches queued raw input.
    pub(crate) fn refresh_ordinary(&mut self,origin: crate::net::Connection,nonce: &str,durable: Has,admitted: Seq) -> Result<(),OrdinaryStageRefusal> {
        ordinary_has_valid(&durable)?;
        let o=self.ordinary_owner(origin,nonce,&durable.stream)?;
        if self.state(&durable.stream)!=durable || admitted<o.admitted || admitted<durable.upto || admitted==u64::MAX || admitted-durable.upto>ORDINARY_WINDOW {return Err(OrdinaryStageRefusal::Domain);}
        let o=self.ordinary.as_mut().ok_or(OrdinaryStageRefusal::Domain)?;
        for (_,item) in o.pending.range_mut(..=admitted) {
            if matches!(&item.effect,OrdinaryEffect::Queued) {item.effect=OrdinaryEffect::AdmittedPending;}
        }
        o.admitted=admitted;Ok(())
    }
    fn ordinary_owner(&self,origin: crate::net::Connection,nonce: &str,stream: &str) -> Result<&OrdinaryIn,OrdinaryStageRefusal> {
        self.ordinary.as_ref().filter(|o|o.origin==origin && o.nonce==nonce && o.stream==stream).ok_or(OrdinaryStageRefusal::Domain)
    }
    pub(crate) fn stage_ordinary(&mut self,origin: crate::net::Connection,nonce: &str,stream: &str,seq: Seq,low: Seq,body: Vec<u8>) -> Result<OrdinaryArrival,OrdinaryStageRefusal> {
        self.stage_ordinary_inner(origin,nonce,stream,(seq,low,body),false)
    }
    /// ONLY an exactly parsed Interrupt/ForceStop may use the reserved class. Original
    /// raw bytes stay unchanged; this reserves input, not permission to dispatch its effect.
    pub(crate) fn stage_ordinary_urgent(&mut self,origin: crate::net::Connection,nonce: &str,stream: &str,seq: Seq,low: Seq,body: Vec<u8>) -> Result<OrdinaryArrival,OrdinaryStageRefusal> {
        self.stage_ordinary_inner(origin,nonce,stream,(seq,low,body),true)
    }
    fn stage_ordinary_inner(&mut self,origin: crate::net::Connection,nonce: &str,stream: &str,item: (Seq,Seq,Vec<u8>),urgent: bool) -> Result<OrdinaryArrival,OrdinaryStageRefusal> {
        let (seq,low,body)=item;
        let o=self.ordinary_owner(origin,nonce,stream)?;
        let has=self.state(stream);
        if seq==0 || seq==u64::MAX {return Err(OrdinaryStageRefusal::Domain);}
        if seq<=has.upto {
            if let Some(o)=self.ordinary.as_mut() {o.low=o.low.max(low.min(has.upto));}
            return Ok(OrdinaryArrival::Answered(Frame::Ack(has)));
        }
        if let Some(item)=o.pending.get(&seq) {
            if item.ticket.body()!=body.as_slice() || item.urgent!=urgent {return Err(OrdinaryStageRefusal::Conflict);}
            let unknown=matches!(&item.effect,OrdinaryEffect::AdmittedPending);
            let o=self.ordinary.as_mut().ok_or(OrdinaryStageRefusal::Domain)?;
            o.low=o.low.max(low.min(has.upto));
            return Ok(if unknown {OrdinaryArrival::AdmittedPending} else {OrdinaryArrival::Duplicate});
        }
        // Admitted without a completed disposition is UNKNOWN, never Take or Taken.
        let admitted=seq<=o.admitted;
        let class_count=o.pending.values().filter(|i|i.urgent==urgent).count();
        let class_bytes:usize=o.pending.values().filter(|i|i.urgent==urgent).map(|i|i.ticket.body.len()+ORDINARY_OUTCOME_ROOM).sum();
        let class_full=if urgent {class_count>=ORDINARY_URGENT_PENDING} else {
            class_count>=ORDINARY_PENDING-ORDINARY_URGENT_PENDING || class_bytes.saturating_add(body.len()).saturating_add(ORDINARY_OUTCOME_ROOM)>super::commands::ROOM-ORDINARY_URGENT_ROOM
        };
        let full=class_full || seq-has.upto>ORDINARY_WINDOW || body.len()>super::commands::ITEM_MOST || o.pending.len()>=ORDINARY_PENDING || o.bytes.saturating_add(body.len()).saturating_add(ORDINARY_OUTCOME_ROOM)>super::commands::ROOM;
        let o=self.ordinary.as_mut().ok_or(OrdinaryStageRefusal::Domain)?;
        o.low=o.low.max(low.min(has.upto));
        if admitted {return Ok(OrdinaryArrival::AdmittedPending);}
        if full {return Err(OrdinaryStageRefusal::Full);}
        let hash=crate::util::sha256_hex(&body);
        let ticket=OrdinaryTicket {origin,nonce:nonce.into(),stream:stream.into(),seq,body:body.into(),hash,authority:std::sync::Arc::clone(&o.authority)};
        let o=self.ordinary.as_mut().ok_or(OrdinaryStageRefusal::Domain)?;
        o.low=o.low.max(low.min(has.upto));o.bytes+=ticket.body.len()+ORDINARY_OUTCOME_ROOM;
        o.pending.insert(seq,OrdinaryItem {ticket,effect:OrdinaryEffect::Queued,urgent});
        let mut through=o.admitted.max(has.upto);
        while through<u64::MAX-1 && o.pending.contains_key(&(through+1)) {through+=1;}
        Ok(if seq>through {OrdinaryArrival::Gap(Frame::Want {stream:stream.into(),from:through+1})} else {OrdinaryArrival::Queued})
    }
    pub(crate) fn ordinary_head(&mut self,origin: crate::net::Connection,nonce: &str,stream: &str) -> Result<Option<OrdinaryTicket>,OrdinaryStageRefusal> {
        self.ordinary_owner(origin,nonce,stream)?;
        let mut seq=self.state(stream).upto.saturating_add(1);
        let o=self.ordinary.as_mut().ok_or(OrdinaryStageRefusal::Domain)?;
        while let Some(item)=o.pending.get_mut(&seq) {
            match &item.effect {
                OrdinaryEffect::Done(_) => seq=seq.saturating_add(1),
                OrdinaryEffect::Claimed | OrdinaryEffect::AdmittedPending => return Ok(None),
                OrdinaryEffect::Queued => {item.effect=OrdinaryEffect::Claimed;return Ok(Some(item.ticket.clone()));},
            }
        }
        Ok(None)
    }
    /// Read-only discovery after an earlier raw gap arrives. Caller captures the exact
    /// Session from SAME body, reserves this ticket, verifies identity, then effects it
    /// without awaiting. No second raw queue or effect is created by peeking.
    pub(crate) fn ordinary_peek_urgent(&self,origin: crate::net::Connection,nonce: &str,stream: &str) -> Result<Option<OrdinaryTicket>,OrdinaryStageRefusal> {
        let o=self.ordinary_owner(origin,nonce,stream)?;
        let mut through=o.admitted.max(self.state(stream).upto);
        while through<u64::MAX-1 && o.pending.contains_key(&(through+1)) {through+=1;}
        Ok(o.pending.range(..=through).find_map(|(_,item)|(item.urgent && matches!(&item.effect,OrdinaryEffect::Queued)).then(||item.ticket.clone())))
    }
    /// Caller classifies urgent control; SAME slot is reserved BEFORE its synchronous effect.
    pub(crate) fn ordinary_urgent(&mut self,origin: crate::net::Connection,nonce: &str,stream: &str,seq: Seq) -> Result<Option<OrdinaryTicket>,OrdinaryStageRefusal> {
        let o=self.ordinary_owner(origin,nonce,stream)?;
        let mut through=o.admitted.max(self.state(stream).upto);
        while through<u64::MAX-1 && o.pending.contains_key(&(through+1)) {through+=1;}
        if seq>through {return Ok(None);}
        let o=self.ordinary.as_mut().ok_or(OrdinaryStageRefusal::Domain)?;
        let Some(item)=o.pending.get_mut(&seq) else {return Ok(None)};
        if !item.urgent || !matches!(&item.effect,OrdinaryEffect::Queued) {return Ok(None);}
        item.effect=OrdinaryEffect::Claimed;Ok(Some(item.ticket.clone()))
    }
    pub(crate) fn ordinary_admitted(&mut self,ticket: &OrdinaryTicket) -> Result<(),OrdinaryStageRefusal> {
        self.ordinary_owner(ticket.origin,&ticket.nonce,&ticket.stream)?;
        let o=self.ordinary.as_mut().ok_or(OrdinaryStageRefusal::Domain)?;
        let item=o.pending.get(&ticket.seq).ok_or(OrdinaryStageRefusal::Domain)?;
        if !item.ticket.same_identity(ticket) || !matches!(&item.effect,OrdinaryEffect::Claimed) || ticket.seq>o.admitted.saturating_add(1) {return Err(OrdinaryStageRefusal::Conflict);}
        o.admitted=o.admitted.max(ticket.seq);Ok(())
    }
    pub(crate) fn ordinary_effect_done(&mut self,ticket: &OrdinaryTicket,refusal: Option<(Refusal,String)>) -> Result<(),OrdinaryStageRefusal> {
        self.ordinary_owner(ticket.origin,&ticket.nonce,&ticket.stream)?;
        let o=self.ordinary.as_mut().ok_or(OrdinaryStageRefusal::Domain)?;
        let item=o.pending.get_mut(&ticket.seq).ok_or(OrdinaryStageRefusal::Domain)?;
        if !item.ticket.same_identity(ticket) {return Err(OrdinaryStageRefusal::Conflict);}
        let refusal=refusal.map(|(code,why)|(code,ordinary_reason(why)));
        match &item.effect {
            OrdinaryEffect::Done(old) if old==&refusal => return Ok(()),
            OrdinaryEffect::Claimed => {},
            _=>return Err(OrdinaryStageRefusal::Conflict),
        }
        item.effect=OrdinaryEffect::Done(refusal);Ok(())
    }
    pub(crate) fn ordinary_ready(&self,origin: crate::net::Connection,nonce: &str,stream: &str) -> Result<Option<OrdinaryClosure>,OrdinaryStageRefusal> {
        let o=self.ordinary_owner(origin,nonce,stream)?;
        let old=self.state(stream);let mut has=old.clone();has.nacks.retain(|n|n.seq>o.low);
        let mut bytes=serde_json::to_vec(&has).map_err(|_|OrdinaryStageRefusal::Conflict)?.len();
        let mut tickets=Vec::new();
        while let Some(item)=o.pending.get(&has.upto.saturating_add(1)) {
            let OrdinaryEffect::Done(refusal)=&item.effect else {break};
            let nack=refusal.as_ref().map(|(code,why)|Nack {seq:item.ticket.seq,code:*code,why:why.clone()});
            let added=nack.as_ref().map(|n|serde_json::to_vec(n).map(|v|v.len()+1)).transpose().map_err(|_|OrdinaryStageRefusal::Conflict)?.unwrap_or(0)+20;
            if (nack.is_some() && has.nacks.len()>=NACKS_KEPT) || bytes.saturating_add(added)>kiln_seams::records::VALUE_MAX-4096 {
                if tickets.is_empty() {return Err(OrdinaryStageRefusal::Full);}break;
            }
            bytes+=added;has.upto=item.ticket.seq;
            if let Some(nack)=nack {has.nacks.push(nack);}
            tickets.push((has.upto,item.ticket.hash.clone()));
        }
        if has.upto==old.upto {return Ok(None);}
        ordinary_has_valid(&has)?;
        Ok(Some(OrdinaryClosure {origin,nonce:nonce.into(),has,low:o.low,base:old.upto,tickets,authority:std::sync::Arc::clone(&o.authority)}))
    }
    /// Called ONLY for the exact captured closure whose Store success is known.
    pub(crate) fn ordinary_committed(&mut self,closure: &OrdinaryClosure) -> Result<Frame,OrdinaryStageRefusal> {
        self.ordinary_owner(closure.origin,&closure.nonce,&closure.has.stream)?;
        if !self.ordinary.as_ref().is_some_and(|o|std::sync::Arc::ptr_eq(&o.authority,&closure.authority)) {return Err(OrdinaryStageRefusal::Domain);}
        let old=self.state(&closure.has.stream);
        if old==closure.has {return Ok(Frame::Ack(old));}
        if old.upto!=closure.base {return Err(OrdinaryStageRefusal::Conflict);}
        let o=self.ordinary.as_mut().ok_or(OrdinaryStageRefusal::Domain)?;
        for (seq,hash) in &closure.tickets {
            let item=o.pending.get(seq).ok_or(OrdinaryStageRefusal::Conflict)?;
            if item.ticket.hash!=*hash || !matches!(&item.effect,OrdinaryEffect::Done(_)) {return Err(OrdinaryStageRefusal::Conflict);}
        }
        for (seq,_) in &closure.tickets {if let Some(item)=o.pending.remove(seq) {
            o.bytes=o.bytes.saturating_sub(item.ticket.body.len()+ORDINARY_OUTCOME_ROOM);
        }}
        o.admitted=o.admitted.max(closure.has.upto);
        let i=self.streams.get_mut(&closure.has.stream).ok_or(OrdinaryStageRefusal::Domain)?;
        i.upto=closure.has.upto;i.nacks=closure.has.nacks.iter().map(|n|(n.seq,(n.code,n.why.clone()))).collect();
        Ok(Frame::Ack(i.has(&closure.has.stream)))
    }
    /// Caller supplies actual original-route retirement, never sender-low or Current alone.
    /// This discards no physical debt and cannot turn old pending work into success.
    pub(crate) fn retire_ordinary(&mut self,origin: crate::net::Connection) -> bool {
        if !self.ordinary.as_ref().is_some_and(|o|o.origin==origin) {return false;}
        if let Some(o)=self.ordinary.take() {self.streams.remove(&o.stream);}true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    fn items(frames: &[Frame]) -> Vec<Seq> {
        frames.iter().filter_map(|f| if let Frame::Item { seq, .. } = f { Some(*seq) } else { None }).collect()
    }

    #[test]
    fn a_have_said_again_on_a_live_link_answers_and_sends_nothing_again() {
        let base = Instant::now();
        let mut tx = Sender::new(1 << 20, Duration::from_secs(10));
        let mut rx = Receiver::new();
        for n in 0..3u8 {
            let _ = tx.push("s", vec![n]);
        }
        // The first Have is lost: nothing goes until the next.
        let _lost = rx.have();
        assert!(tx.poll(base).is_empty());
        tx.on_frame(rx.have());
        let first = tx.poll(base);
        assert_eq!(items(&first), vec![1, 2, 3]);
        // The receiver took the first two; its Acks are lost, and it says what it has again.
        let _acks = deliver(&mut rx, first.into_iter().take(2).collect());
        let events = tx.on_frame(rx.have());
        assert_eq!(events.len(), 2, "answered by the Have: {events:?}");
        assert!(tx.poll(t(base, 10)).is_empty(), "nothing goes again before its patience is up");
        assert_eq!(items(&tx.poll(t(base, 10_001))), vec![3], "only the one still unanswered, once its patience is up");
        // A Have made before an answer that came ahead of it says less than was answered: on a
        // live link that's no sign the receiver lost what it had.
        let stale = rx.have();
        for a in deliver(&mut rx, tx.poll(t(base, 20_002))) {
            tx.on_frame(a);
        }
        let events = tx.on_frame(stale);
        assert!(!events.iter().any(|e| matches!(e, Event::Regressed { .. })), "{events:?}");
    }

    /// A sender's frames, applied by a receiver that takes everything: its answers.
    fn deliver(r: &mut Receiver, frames: Vec<Frame>) -> Vec<Frame> {
        let mut back = Vec::new();
        for f in frames {
            if let Frame::Item { stream, seq, low, body } = f {
                match r.on_item(&stream, seq, low, body) {
                    Arrival::Take(_) => back.push(r.took(&stream, seq)),
                    Arrival::Answered(Some(a)) | Arrival::Gap(a) => back.push(a),
                    Arrival::Answered(None) => {}
                }
            }
        }
        back
    }

    #[test]
    fn nothing_goes_until_the_peer_says_what_it_has() -> anyhow::Result<()> {
        let base = Instant::now();
        let mut s = Sender::new(1 << 20, Duration::from_secs(10));
        s.push("a", b"one".to_vec()).map_err(|_| anyhow::anyhow!("full"))?;
        assert!(s.poll(base).is_empty());
        s.on_frame(Frame::Have(Vec::new()));
        assert_eq!(items(&s.poll(base)), vec![1]);
        // Sent once; again only when its answer is overdue.
        assert!(s.poll(t(base, 5_000)).is_empty());
        assert_eq!(items(&s.poll(t(base, 10_000))), vec![1]);
        Ok(())
    }

    #[test]
    fn a_reconnect_sends_only_what_the_receiver_lacks() -> anyhow::Result<()> {
        let base = Instant::now();
        let mut s = Sender::new(1 << 20, Duration::from_secs(10));
        let mut r = Receiver::new();
        s.on_frame(r.have());
        for n in 0..5u8 {
            s.push("a", vec![n]).map_err(|_| anyhow::anyhow!("full"))?;
        }
        let sent = s.poll(base);
        // The first three arrive and are taken; the link drops before any answer comes back.
        let _lost_answers = deliver(&mut r, sent.into_iter().take(3).collect());
        s.down();
        assert!(s.poll(base).is_empty());
        let events = s.on_frame(r.have());
        assert_eq!(events.len(), 3, "the three taken are answered by what the receiver has: {events:?}");
        assert_eq!(items(&s.poll(base)), vec![4, 5]);
        Ok(())
    }

    #[test]
    fn u03_storage_failure_replays_without_acknowledging_or_skipping() {
        let mut receiver = Receiver::new();
        let body = b"completion".to_vec();
        assert_eq!(receiver.on_item("job", 1, 0, body.clone()), Arrival::Take(body.clone()));
        receiver.retry("job", 2);
        assert_eq!(receiver.on_item("job", 1, 0, body.clone()), Arrival::Answered(None));
        receiver.retry("job", 1);
        assert_eq!(receiver.state("job").upto, 0);
        assert_eq!(receiver.on_item("job", 1, 0, body.clone()), Arrival::Take(body.clone()));
        let answer = receiver.took("job", 1);
        assert_eq!(receiver.on_item("job", 1, 0, body), Arrival::Answered(Some(answer)));
    }

    #[test]
    fn a_copy_is_taken_once_and_answered_again() {
        let mut r = Receiver::new();
        assert_eq!(r.on_item("a", 1, 0, b"x".to_vec()), Arrival::Take(b"x".to_vec()));
        // A copy while it's being applied: dropped; its answer comes once it's applied.
        assert_eq!(r.on_item("a", 1, 0, b"x".to_vec()), Arrival::Answered(None));
        let first = r.took("a", 1);
        // A copy after: the same answer again, nothing applied.
        assert_eq!(r.on_item("a", 1, 0, b"x".to_vec()), Arrival::Answered(Some(first)));
    }

    #[test]
    fn a_refusal_is_never_taken_for_a_taking() -> anyhow::Result<()> {
        let base = Instant::now();
        let mut s = Sender::new(1 << 20, Duration::from_secs(10));
        let mut r = Receiver::new();
        s.on_frame(r.have());
        s.push("a", b"bad".to_vec()).map_err(|_| anyhow::anyhow!("full"))?;
        s.push("a", b"good".to_vec()).map_err(|_| anyhow::anyhow!("full"))?;
        let mut sent = s.poll(base).into_iter();
        let (Some(Frame::Item { seq: s1, low: l1, body: b1, .. }), Some(Frame::Item { seq: s2, low: l2, body: b2, .. })) = (sent.next(), sent.next()) else {
            anyhow::bail!("two items");
        };
        assert!(matches!(r.on_item("a", s1, l1, b1), Arrival::Take(_)));
        let _lost = r.refused("a", s1, Refusal::Unknown, "no such approval");
        assert!(matches!(r.on_item("a", s2, l2, b2), Arrival::Take(_)));
        // The refusal's own answer was lost: the next answer still carries it.
        let events = s.on_frame(r.took("a", s2));
        assert_eq!(
            events,
            vec![
                Event::Answered { stream: "a".into(), seq: 1, outcome: Outcome::Refused { code: Refusal::Unknown, why: "no such approval".into() } },
                Event::Answered { stream: "a".into(), seq: 2, outcome: Outcome::Taken },
            ]
        );
        // U01/U03: held durability cannot block one urgent effect, nor advertise it.
        let origin=crate::net::Connection::new(1,1,[1;32]);let nonce="a".repeat(43);let stream="l:7.1";
        let mut staged=Receiver::new();
        staged.bind_ordinary(origin,&nonce,Has {stream:stream.into(),upto:0,nacks:vec![]},0)?;
        staged.stage_ordinary(origin,&nonce,stream,1,0,b"held Start".to_vec())?;
        staged.stage_ordinary_urgent(origin,&nonce,stream,2,0,b"urgent".to_vec())?;
        let held=staged.ordinary_head(origin,&nonce,stream)?.ok_or_else(||anyhow::anyhow!("held head missing"))?;
        let urgent=staged.ordinary_urgent(origin,&nonce,stream,2)?.ok_or_else(||anyhow::anyhow!("urgent not reserved"))?;
        assert!(staged.ordinary_urgent(origin,&nonce,stream,2)?.is_none(),"duplicate cannot dispatch ForceStop twice");
        staged.ordinary_effect_done(&urgent,Some((Refusal::Unknown,"exact refusal".into())))?;
        assert!(staged.ordinary_ready(origin,&nonce,stream)?.is_none(),"cannot ACK past unresolved head");
        assert_eq!(staged.state(stream).upto,0);
        assert_eq!(staged.have(),Frame::Have(vec![Has {stream:stream.into(),upto:0,nacks:vec![]} ]));
        assert_eq!(staged.stage_ordinary_urgent(origin,&nonce,stream,2,100,b"urgent".to_vec())?,OrdinaryArrival::Duplicate);
        assert!(staged.stage_ordinary_urgent(origin,&nonce,stream,2,0,b"changed".to_vec()).is_err());
        staged.refresh_ordinary(origin,&nonce,staged.state(stream),1)?;
        staged.ordinary_admitted(&held)?;
        staged.ordinary_effect_done(&held,None)?;
        let closure=staged.ordinary_ready(origin,&nonce,stream)?.ok_or_else(||anyhow::anyhow!("completed prefix missing"))?;
        assert_eq!(staged.state(stream).upto,0,"preparing or holding a Store future is not commit");
        let saved=closure.has().clone();let _lost=staged.ordinary_committed(&closure)?;
        assert_eq!(saved.upto,2);assert_eq!(saved.nacks.len(),1);
        let mut restored=Receiver::new();restored.bind_ordinary(origin,&nonce,saved.clone(),2)?;
        assert_eq!(restored.stage_ordinary_urgent(origin,&nonce,stream,2,0,b"urgent".to_vec())?,OrdinaryArrival::Answered(Frame::Ack(saved)));
        let mut unknown=Receiver::new();unknown.bind_ordinary(origin,&nonce,Has {stream:stream.into(),upto:0,nacks:vec![]},1)?;
        assert_eq!(unknown.stage_ordinary(origin,&nonce,stream,1,0,b"Start".to_vec())?,OrdinaryArrival::AdmittedPending);
        assert!(unknown.ordinary_head(origin,&nonce,stream)?.is_none());
        let mut refreshed=Receiver::new();refreshed.bind_ordinary(origin,&nonce,Has {stream:stream.into(),upto:0,nacks:vec![]},0)?;
        refreshed.stage_ordinary(origin,&nonce,stream,1,0,b"unexposed raw".to_vec())?;
        refreshed.refresh_ordinary(origin,&nonce,refreshed.state(stream),1)?;
        assert_eq!(refreshed.stage_ordinary(origin,&nonce,stream,1,0,b"unexposed raw".to_vec())?,OrdinaryArrival::AdmittedPending);
        assert!(refreshed.ordinary_head(origin,&nonce,stream)?.is_none());
        assert!(refreshed.refresh_ordinary(origin,&nonce,refreshed.state(stream),0).is_err());
        assert_eq!(refreshed.state(stream).upto,0,"positive Gate admission is not wire Taken");
        assert!(staged.retire_ordinary(origin));
        assert!(staged.ordinary_committed(&closure).is_err(),"late original closure cannot ACK a replacement");
        staged.bind_ordinary(origin,&nonce,Has {stream:stream.into(),upto:0,nacks:vec![]},0)?;
        staged.stage_ordinary(origin,&nonce,stream,1,0,b"held Start".to_vec())?;
        let rebound=staged.ordinary_head(origin,&nonce,stream)?.ok_or_else(||anyhow::anyhow!("rebound ticket missing"))?;
        assert!(!held.same_identity(&rebound));assert!(held.same_identity(&held.clone()));
        assert!(staged.ordinary_effect_done(&held,None).is_err(),"old opaque ticket cannot settle a later binding");
        staged.ordinary_effect_done(&rebound,Some((Refusal::Malformed,"é".repeat(ORDINARY_WHY_MAX))))?;
        let shortened=staged.ordinary_ready(origin,&nonce,stream)?.ok_or_else(||anyhow::anyhow!("bounded refusal missing"))?;
        assert!(shortened.has().nacks.iter().all(|n|n.code==Refusal::Malformed && n.why.len()<=ORDINARY_WHY_MAX && n.why.ends_with(" [truncated]")));
        let mut bounded=Receiver::new();bounded.bind_ordinary(origin,&nonce,Has {stream:stream.into(),upto:0,nacks:vec![]},0)?;
        for seq in 1..=(ORDINARY_PENDING-ORDINARY_URGENT_PENDING) as u64 {bounded.stage_ordinary(origin,&nonce,stream,seq,0,vec![1])?;}
        assert_eq!(bounded.stage_ordinary(origin,&nonce,stream,1000,0,vec![1]),Err(OrdinaryStageRefusal::Full));
        bounded.stage_ordinary_urgent(origin,&nonce,stream,63,0,vec![2])?;bounded.stage_ordinary_urgent(origin,&nonce,stream,64,0,vec![3])?;
        assert_eq!(bounded.stage_ordinary_urgent(origin,&nonce,stream,65,0,vec![4]),Err(OrdinaryStageRefusal::Full));
        let mut bytes=Receiver::new();bytes.bind_ordinary(origin,&nonce,Has {stream:stream.into(),upto:0,nacks:vec![]},0)?;
        bytes.stage_ordinary(origin,&nonce,stream,1,0,vec![0;super::super::commands::ITEM_MOST])?;
        let remainder=super::super::commands::ROOM-ORDINARY_URGENT_ROOM-super::super::commands::ITEM_MOST-2*ORDINARY_OUTCOME_ROOM;
        bytes.stage_ordinary(origin,&nonce,stream,2,0,vec![0;remainder])?;
        assert_eq!(bytes.stage_ordinary(origin,&nonce,stream,5,0,vec![0]),Err(OrdinaryStageRefusal::Full));
        let effect=bytes.ordinary_head(origin,&nonce,stream)?.ok_or_else(||anyhow::anyhow!("reserved outcome missing"))?;
        bytes.stage_ordinary_urgent(origin,&nonce,stream,3,0,vec![0;super::super::commands::ITEM_MOST])?;
        bytes.stage_ordinary_urgent(origin,&nonce,stream,4,0,vec![0;super::super::commands::ITEM_MOST])?;
        assert_eq!(bytes.stage_ordinary_urgent(origin,&nonce,stream,5,0,vec![0]),Err(OrdinaryStageRefusal::Full));
        let peek=bytes.ordinary_peek_urgent(origin,&nonce,stream)?.ok_or_else(||anyhow::anyhow!("reserved urgent missing"))?;
        let urgent=bytes.ordinary_urgent(origin,&nonce,stream,peek.seq())?.ok_or_else(||anyhow::anyhow!("reserved urgent unavailable"))?;
        assert!(peek.same_identity(&urgent));
        assert_eq!(bytes.stage_ordinary_urgent(origin,&nonce,stream,3,0,vec![0;super::super::commands::ITEM_MOST])?,OrdinaryArrival::Duplicate);
        assert!(bytes.ordinary_urgent(origin,&nonce,stream,3)?.is_none());
        bytes.ordinary_effect_done(&urgent,None)?;
        let second=bytes.ordinary_peek_urgent(origin,&nonce,stream)?.ok_or_else(||anyhow::anyhow!("second reserved urgent missing"))?;
        assert_eq!(second.seq(),4);
        let second_reserved=bytes.ordinary_urgent(origin,&nonce,stream,second.seq())?.ok_or_else(||anyhow::anyhow!("second reserved control unavailable"))?;
        assert!(second.same_identity(&second_reserved));bytes.ordinary_effect_done(&second_reserved,None)?;
        assert!(bytes.ordinary_ready(origin,&nonce,stream)?.is_none());
        bytes.ordinary_effect_done(&effect,Some((Refusal::Refused,"x".repeat(ORDINARY_WHY_MAX*2))))?;
        assert!(bytes.ordinary_ready(origin,&nonce,stream)?.is_some(),"full raw input already reserved its known outcome");
        let mut gap=Receiver::new();gap.bind_ordinary(origin,&nonce,Has {stream:stream.into(),upto:0,nacks:vec![]},0)?;
        gap.stage_ordinary(origin,&nonce,stream,1,0,b"held".to_vec())?;let _head=gap.ordinary_head(origin,&nonce,stream)?;
        assert!(matches!(gap.stage_ordinary_urgent(origin,&nonce,stream,3,0,b"urgent".to_vec())?,OrdinaryArrival::Gap(_)));
        assert!(gap.ordinary_peek_urgent(origin,&nonce,stream)?.is_none());
        gap.stage_ordinary(origin,&nonce,stream,2,0,b"earlier raw".to_vec())?;
        assert!(gap.ordinary_peek_urgent(origin,&nonce,stream)?.is_some(),"gap arrival exposes SAME retained urgent without resend");
        Ok(())
    }

    #[test]
    fn a_gap_asks_for_what_is_missing() {
        let mut r = Receiver::new();
        assert_eq!(r.on_item("a", 2, 0, Vec::new()), Arrival::Gap(Frame::Want { stream: "a".into(), from: 1 }));
        let base = Instant::now();
        let mut s = Sender::new(1 << 20, Duration::from_secs(10));
        s.on_frame(Frame::Have(Vec::new()));
        for n in 0..3u8 {
            let _ = s.push("a", vec![n]);
        }
        assert_eq!(items(&s.poll(base)), vec![1, 2, 3]);
        s.on_frame(Frame::Want { stream: "a".into(), from: 2 });
        assert_eq!(items(&s.poll(base)), vec![2, 3]);
    }

    /// Each item past a gap brings a want: the receiver asks once, and its sender sends what
    /// follows the gap again once, not once for each.
    #[test]
    fn a_gap_is_asked_for_and_sent_again_once() -> anyhow::Result<()> {
        let mut r = Receiver::new();
        assert!(matches!(r.on_item("a", 1, 0, Vec::new()), Arrival::Take(_)));
        r.took("a", 1);
        assert_eq!(r.on_item("a", 3, 0, Vec::new()), Arrival::Gap(Frame::Want { stream: "a".into(), from: 2 }));
        assert_eq!(r.on_item("a", 4, 0, Vec::new()), Arrival::Answered(None), "asked for once");
        // Asked again only after many more past it (that want lost below the channel).
        let wants = (5..200).filter(|n| matches!(r.on_item("a", *n, 0, Vec::new()), Arrival::Gap(_))).count();
        assert_eq!(wants, 3, "once every 64 past the gap");
        assert!(matches!(r.on_item("a", 2, 0, Vec::new()), Arrival::Take(_)));
        r.took("a", 2);
        // A gap further on is asked for in its turn.
        assert_eq!(r.on_item("a", 5, 0, Vec::new()), Arrival::Gap(Frame::Want { stream: "a".into(), from: 3 }));

        let base = Instant::now();
        let mut s = Sender::new(1 << 20, Duration::from_secs(10));
        s.on_frame(Frame::Have(Vec::new()));
        for n in 0..4u8 {
            s.push("a", vec![n]).map_err(|_| anyhow::anyhow!("full"))?;
        }
        assert_eq!(items(&s.poll(base)), vec![1, 2, 3, 4]);
        // An older receiver's wants, one for each of 3 and 4 past a lost 2.
        s.on_frame(Frame::Want { stream: "a".into(), from: 2 });
        assert_eq!(items(&s.poll(t(base, 1))), vec![2, 3, 4], "the first want: at once");
        s.on_frame(Frame::Want { stream: "a".into(), from: 2 });
        assert!(s.poll(t(base, 2)).is_empty(), "one made before they got there again: not yet");
        assert_eq!(items(&s.poll(t(base, 1 + 250))), vec![2, 3, 4], "once WANT_AGAIN has passed");
        assert!(s.poll(t(base, 600)).is_empty());
        Ok(())
    }

    /// A receiver that started again with nothing kept, while its sender's stream went on (it
    /// didn't hear the receiver say so): it goes on from its sender's `low`, and asks for nothing
    /// before it.
    #[test]
    fn a_receiver_behind_its_senders_low_goes_on_from_there() {
        let mut r = Receiver::new();
        assert!(matches!(r.on_item("p:7.1", 41, 40, b"x".to_vec()), Arrival::Take(_)));
        assert_eq!(r.took("p:7.1", 41), Frame::Ack(Has { stream: "p:7.1".into(), upto: 41, nacks: Vec::new() }));
        // One answered already, by the one before it: answered so again.
        assert!(matches!(r.on_item("p:7.1", 39, 40, Vec::new()), Arrival::Answered(Some(_))));
    }

    #[test]
    fn a_full_stream_makes_its_producer_wait() {
        let mut s = Sender::new(10, Duration::from_secs(10));
        assert!(s.push("a", vec![0; 8]).is_ok());
        assert_eq!(s.push("a", vec![0; 8]), Err(Full));
        // Another stream has room of its own; an item larger than the room goes alone.
        assert!(s.push("b", vec![0; 64]).is_ok());
        s.on_frame(Frame::Have(Vec::new()));
        s.on_frame(Frame::Ack(Has { stream: "a".into(), upto: 1, nacks: Vec::new() }));
        assert!(s.push("a", vec![0; 8]).is_ok());
    }

    /// A stream's picture goes whatever its size: one larger than the room could never start its
    /// stream. The room bounds what's pushed after it, numbered after it.
    #[test]
    fn a_picture_larger_than_the_room_goes_and_the_room_bounds_what_follows() {
        let mut s = Sender::new(10, Duration::from_secs(10));
        let picture: Vec<Seq> = (0..4).map(|_| s.push_picture("a", vec![0; 8])).collect();
        assert_eq!(picture, vec![1, 2, 3, 4]);
        assert_eq!((s.bytes(), s.backlog()), (32, 0));
        assert_eq!(s.push("a", vec![0; 8]), Ok(5), "the room is what follows the picture's");
        assert_eq!(s.push("a", vec![0; 8]), Err(Full));
        // The picture and what followed it answered: room again, and nothing kept.
        s.on_frame(Frame::Have(Vec::new()));
        s.on_frame(Frame::Ack(Has { stream: "a".into(), upto: 5, nacks: Vec::new() }));
        assert_eq!((s.bytes(), s.backlog()), (0, 0));
        assert_eq!(s.push("a", vec![0; 8]), Ok(6));
    }

    #[test]
    fn a_receiver_that_lost_what_it_kept_is_reported() -> anyhow::Result<()> {
        let base = Instant::now();
        let mut s = Sender::new(1 << 20, Duration::from_secs(10));
        let mut r = Receiver::new();
        s.on_frame(r.have());
        for n in 0..3u8 {
            s.push("a", vec![n]).map_err(|_| anyhow::anyhow!("full"))?;
        }
        for a in deliver(&mut r, s.poll(base)) {
            s.on_frame(a);
        }
        // It restarted with nothing kept.
        let fresh = Receiver::new();
        s.down();
        let events = s.on_frame(fresh.have());
        assert_eq!(events, vec![Event::Regressed { stream: "a".into(), has: 0, answered: 3 }]);
        Ok(())
    }

    #[test]
    fn a_durable_stream_goes_on_after_its_sender_restarts() -> anyhow::Result<()> {
        let base = Instant::now();
        let mut s = Sender::new(1 << 20, Duration::from_secs(10));
        s.on_frame(Frame::Have(Vec::new()));
        for n in 0..4u8 {
            s.push("a", vec![n]).map_err(|_| anyhow::anyhow!("full"))?;
        }
        s.on_frame(Frame::Ack(Has { stream: "a".into(), upto: 2, nacks: Vec::new() }));
        // Restarted: what it kept was items 3 and 4, and 5 is next.
        let mut again = Sender::new(1 << 20, Duration::from_secs(10));
        again.restore("a", 5, vec![(3, vec![2]), (4, vec![3])]);
        again.on_frame(Frame::Have(vec![Has { stream: "a".into(), upto: 2, nacks: Vec::new() }]));
        assert_eq!(items(&again.poll(base)), vec![3, 4]);
        assert_eq!(again.push("a", vec![9]), Ok(5));
        let mut fair=Sender::new(1024,Duration::from_secs(10));
        for stream in ["held","other"]{for n in 0..4u8{fair.push(stream,vec![n]).map_err(|_|anyhow::anyhow!("fixture window full"))?;}}
        fair.on_frame(Frame::Have(Vec::new()));
        let mut after=None;let mut observed=std::collections::BTreeSet::new();
        for _ in 0..8{let frames=fair.poll_bounded(base,1,&mut after);assert_eq!(frames.len(),1);for frame in frames{if let Frame::Item{stream,seq,..}=frame{observed.insert((stream,seq));}}}
        assert_eq!(observed.len(),8,"bounded turns do not repeatedly timestamp unsent later items");
        assert!(fair.poll_bounded(base,1,&mut after).is_empty());
        assert_eq!(fair.unanswered(),8,"transport attempts without own receipts keep every original");
        Ok(())
    }

    /// The channel under faults: every item applied once, in order, and answered once as it was.
    mod sim {
        use std::collections::BTreeSet;

        use super::*;

        /// SplitMix64: deterministic, so every schedule is replayed from its seed.
        struct Rng(u64);

        impl Rng {
            fn next(&mut self) -> u64 {
                self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
                let mut z = self.0;
                z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                z ^ (z >> 31)
            }

            /// True one time in `n`.
            fn one_in(&mut self, n: u64) -> bool {
                self.next().is_multiple_of(n.max(1))
            }

            fn below(&mut self, n: u64) -> u64 {
                self.next() % n.max(1)
            }
        }

        const STREAMS: [&str; 3] = ["a", "b", "c"];

        /// Whether the receiver refuses an item: decided by its body, so its sender's answer can
        /// be checked against it.
        fn refuses(body: &[u8]) -> bool {
            body.first().is_some_and(|b| b % 7 == 0)
        }

        struct Run {
            applied: BTreeMap<String, Vec<Seq>>,
            answers: BTreeMap<(String, Seq), Vec<Outcome>>,
            bodies: BTreeMap<(String, Seq), Vec<u8>>,
            regressed: usize,
        }

        /// One schedule: `count` items a stream through a link that drops, copies, delays,
        /// reorders and cuts, with both sides restarting (durably) now and then.
        fn run(seed: u64, count: u64) -> Run {
            let mut rng = Rng(seed);
            let base = Instant::now();
            let room = 64;
            let patience = Duration::from_millis(400);
            let mut s = Sender::new(room, patience);
            let mut r = Receiver::new();
            // What each durable side kept: the sender's unanswered items, the receiver's state.
            let mut kept_out: BTreeMap<String, BTreeMap<Seq, Vec<u8>>> = BTreeMap::new();
            let mut kept_next: BTreeMap<String, Seq> = BTreeMap::new();
            let mut kept_in: BTreeMap<String, Has> = BTreeMap::new();
            let mut pushed: BTreeMap<String, u64> = BTreeMap::new();
            let mut to_r: Vec<(u64, Frame)> = Vec::new();
            let mut to_s: Vec<(u64, Frame)> = Vec::new();
            let mut up = true;
            let mut out = Run { applied: BTreeMap::new(), answers: BTreeMap::new(), bodies: BTreeMap::new(), regressed: 0 };
            to_s.push((0, r.have()));
            let faulty_until = 1_200;
            for tick in 0..40_000u64 {
                let now = base + Duration::from_millis(tick * 10);
                let faults = tick < faulty_until;
                // The producer: items while there's room.
                for stream in STREAMS {
                    let n = pushed.entry(stream.to_owned()).or_default();
                    if *n < count && rng.one_in(30) {
                        let body = vec![u8::try_from(rng.below(256)).unwrap_or(0)];
                        if let Ok(seq) = s.push(stream, body.clone()) {
                            *n += 1;
                            out.bodies.insert((stream.to_owned(), seq), body.clone());
                            kept_out.entry(stream.to_owned()).or_default().insert(seq, body);
                            kept_next.insert(stream.to_owned(), seq + 1);
                        }
                    }
                }
                // The link: a cut, then up again after a while, the receiver saying what it has.
                if faults && up && rng.one_in(150) {
                    up = false;
                    to_r.clear();
                    to_s.clear();
                    s.down();
                    r.down();
                } else if !up && rng.one_in(8) {
                    up = true;
                    to_s.push((tick, r.have()));
                }
                // Restarts: either side, from what it kept.
                if faults && rng.one_in(900) {
                    let mut again = Sender::new(room, patience);
                    for stream in STREAMS {
                        let pending: Vec<(Seq, Vec<u8>)> = kept_out.get(stream).map(|m| m.iter().map(|(k, v)| (*k, v.clone())).collect()).unwrap_or_default();
                        let next = kept_next.get(stream).copied().unwrap_or(1);
                        again.restore(stream, next, pending);
                    }
                    s = again;
                    if up {
                        to_s.push((tick, r.have()));
                    }
                }
                if faults && rng.one_in(900) {
                    let mut again = Receiver::new();
                    for has in kept_in.values() {
                        again.restore(has.clone());
                    }
                    r = again;
                    if up {
                        to_s.push((tick, r.have()));
                    }
                }
                if up {
                    for f in s.poll(now) {
                        if faults && rng.one_in(12) {
                            continue;
                        }
                        let delay = if faults { rng.below(6) } else { 0 };
                        if faults && rng.one_in(20) {
                            to_r.push((tick + rng.below(6), f.clone()));
                        }
                        to_r.push((tick + delay, f));
                    }
                }
                // Deliver what's due, in an order the delays made.
                let (due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut to_r).into_iter().partition(|(at, _)| *at <= tick);
                to_r = later;
                for (_, f) in due {
                    let Frame::Item { stream, seq, low, body } = f else { continue };
                    let answer = match r.on_item(&stream, seq, low, body.clone()) {
                        Arrival::Take(_) => {
                            out.applied.entry(stream.clone()).or_default().push(seq);
                            let a = if refuses(&body) { r.refused(&stream, seq, Refusal::Refused, "seven") } else { r.took(&stream, seq) };
                            kept_in.insert(stream.clone(), r.state(&stream));
                            Some(a)
                        }
                        Arrival::Answered(a) => a,
                        Arrival::Gap(w) => Some(w),
                    };
                    if let Some(a) = answer
                        && up
                        && !(faults && rng.one_in(12))
                    {
                        to_s.push((tick + if faults { rng.below(6) } else { 0 }, a));
                    }
                }
                let (due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut to_s).into_iter().partition(|(at, _)| *at <= tick);
                to_s = later;
                for (_, f) in due {
                    for e in s.on_frame(f) {
                        match e {
                            Event::Answered { stream, seq, outcome } => {
                                if let Some(m) = kept_out.get_mut(&stream) {
                                    m.remove(&seq);
                                }
                                out.answers.entry((stream, seq)).or_default().push(outcome);
                            }
                            Event::Regressed { .. } => out.regressed += 1,
                        }
                    }
                }
                let all_pushed = STREAMS.iter().all(|st| pushed.get(*st).copied().unwrap_or(0) >= count);
                if !faults && all_pushed && s.unanswered() == 0 {
                    break;
                }
            }
            out
        }

        /// What went wrong in one schedule, if anything.
        fn check(seed: u64, count: u64) -> Option<String> {
            let run = run(seed, count);
            for stream in STREAMS {
                let applied = run.applied.get(stream).cloned().unwrap_or_default();
                let want: Vec<Seq> = (1..=count).collect();
                if applied != want {
                    let seen: BTreeSet<Seq> = applied.iter().copied().collect();
                    return Some(format!("seed {seed}: stream {stream} applied {} items ({} distinct), want 1..={count} once each in order", applied.len(), seen.len()));
                }
                for seq in 1..=count {
                    let key = (stream.to_owned(), seq);
                    let refused = run.bodies.get(&key).is_some_and(|b| refuses(b));
                    match run.answers.get(&key).map(Vec::as_slice) {
                        Some([Outcome::Taken]) if !refused => {}
                        Some([Outcome::Refused { .. }]) if refused => {}
                        Some([one]) => return Some(format!("seed {seed}: {stream}#{seq} answered {one:?}, but the receiver {}", if refused { "refused it" } else { "took it" })),
                        Some(a) => return Some(format!("seed {seed}: {stream}#{seq} answered {} times", a.len())),
                        None => return Some(format!("seed {seed}: {stream}#{seq} never answered")),
                    }
                }
            }
            if run.regressed > 0 {
                return Some(format!("seed {seed}: a durable receiver regressed {} times", run.regressed));
            }
            None
        }

        #[test]
        fn nothing_is_lost_or_doubled_across_ten_thousand_schedules() {
            let mut failed = Vec::new();
            for seed in 0..10_000u64 {
                if let Some(why) = check(seed, 12) {
                    failed.push(why);
                    if failed.len() >= 5 {
                        break;
                    }
                }
            }
            assert!(failed.is_empty(), "replay a schedule with its seed:\n{}", failed.join("\n"));
        }

        #[test]
        fn a_long_schedule_holds_and_refuses_some() {
            assert_eq!(check(7, 200), None);
            let run = run(7, 200);
            assert!(run.answers.values().flatten().any(|o| matches!(o, Outcome::Refused { .. })), "some of 600 items are refused");
        }
    }
}
