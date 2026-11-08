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
//!   gap: once, not for each item past it, and again while it's still there (that want, or what it
//!   brought, lost below the channel): every [`WANT_EVERY`] items past it, or, for a stream with
//!   few items in flight ([`Receiver::timed`]: a runner's of its server's commands), once
//!   [`ASK_AGAIN`] has passed, as more items past it come and on its own clock
//!   ([`Receiver::wants`]). Its sender sends that again at once, and a want that comes after (made
//!   before the receiver had them again, one for each item that was past the gap) sends it again
//!   only once [`WANT_AGAIN`] has passed. A receiver that's behind its sender's `low` (it started
//!   again with less than that) goes on from there: every item through it is answered.
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
use std::sync::Arc;
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
    /// Its stream's name as its receiver keeps it, shared: each answer's is that one, not a copy (goal 9, plan
    /// 01a1a87b2cec step 3).
    #[serde(with = "crate::proto::shared_str")]
    pub stream: Arc<str>,
    pub upto: Seq,
    pub nacks: Vec<Nack>,
}

/// A frame of the channel, either way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Frame {
    /// Item `seq` of `stream`. `low`: its sender has every answer through it. `body`: the message
    /// it carries, encoded as its wire encodes it: read whole as postcard's bytes, the same bytes on the
    /// wire as a sequence of `u8` (`crate::proto::wire_bytes`; goal 9, plan 01a1a87b2cec step 5: rt7's
    /// shards read every runner's channel item a byte a call).
    Item {
        stream: String,
        seq: Seq,
        low: Seq,
        #[serde(with = "crate::proto::wire_bytes")]
        body: Vec<u8>,
    },
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

/// How long an item waits for its answer before it goes again: its stream's retransmission timeout
/// as RFC 6298 §2 has it, RTO = SRTT + K·RTTVAR (K = 4), from how long its answers take (smoothed
/// with the RFC's gains, α = 1/8 for SRTT and β = 1/4 for RTTVAR) and from items sent once alone
/// (Karn's algorithm, §3), never less than the stream's patience (as §2.4 bounds RTO from below).
/// An answer that's late is mostly a receiver that's slow, not a frame lost: sent again at each
/// patience, a slow receiver's whole backlog (a session's snapshot, megabytes) made it slower.
fn rto(srtt: Option<Duration>, rttvar: Duration, patience: Duration) -> Duration {
    srtt.map_or(patience, |srtt| srtt.saturating_add(rttvar.saturating_mul(4))).max(patience)
}

/// SRTT and RTTVAR after an answer that came `r` after its item went (RFC 6298 §2.2 for the
/// first, §2.3 after it).
fn sampled(srtt: Option<Duration>, rttvar: Duration, r: Duration) -> (Option<Duration>, Duration) {
    match srtt {
        None => (Some(r), r / 2),
        Some(srtt) => {
            let off = if srtt > r { srtt.saturating_sub(r) } else { r.saturating_sub(srtt) };
            (Some(srtt.saturating_sub(srtt / 8).saturating_add(r / 8)), rttvar.saturating_sub(rttvar / 4).saturating_add(off / 4))
        }
    }
}

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
    /// It went more than once: its answer can't say how long one takes (Karn's algorithm).
    resent: bool,
}

impl Pending {
    fn new(seq: Seq, body: Vec<u8>, picture: bool) -> Self {
        Self { seq, body, picture, sent: None, wanted: Wanted::No, went_for_want: None, resent: false }
    }

    /// Not sent on this link yet: it goes as if new.
    fn unsent(&mut self) {
        self.resent |= self.sent.is_some();
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
    /// How long its answers take, smoothed, and how much that varies (RFC 6298 §2): none before its
    /// first answer to an item sent once.
    srtt: Option<Duration>,
    rttvar: Duration,
    /// When each item answered since the last poll went, of those sent once: what its answer took
    /// is measured at the next poll (this side reads no clock but `poll`'s).
    answered_sent: Vec<Instant>,
}

impl Default for Out {
    fn default() -> Self {
        Self { next: 1, answered: 0, pending: VecDeque::new(), bytes: 0, picture: 0, srtt: None, rttvar: Duration::ZERO, answered_sent: Vec::new() }
    }
}

impl Out {
    /// What an answer that came `r` after its item went says (RFC 6298 §2.2 for the first, §2.3
    /// after it).
    fn sample(&mut self, r: Duration) {
        (self.srtt, self.rttvar) = sampled(self.srtt, self.rttvar, r);
    }

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
            for at in std::mem::take(&mut o.answered_sent) {
                o.sample(now.saturating_duration_since(at));
            }
            let wait = rto(o.srtt, o.rttvar, self.patience);
            let low = o.answered;
            for p in &mut o.pending {
                let due = match p.sent {
                    None => true,
                    Some(at) if now.saturating_duration_since(at) >= wait => true,
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
                    p.resent |= p.sent.is_some();
                    p.wanted = Wanted::No;
                    p.sent = Some(now);
                    out.push(Frame::Item { stream: stream.clone(), seq: p.seq, low, body: p.body.clone() });
                }
            }
        }
        out
    }

    /// When [`Sender::poll`] next has something to send on its own clock, as of `now`: an item not
    /// sent yet or wanted again at once (now), the earliest sent item's answer overdue (its stream's
    /// RTO after it went, as `poll` reckons it, its answers' samples taken), or a want's again; none
    /// while the link is down or nothing waits. What the peer's word frees (an answer, a want) is
    /// the caller's to poll for as it comes (goal 9: a due item, never a tick).
    pub fn due(&self, now: Instant) -> Option<Instant> {
        if !self.up {
            return None;
        }
        let mut due: Option<Instant> = None;
        for o in self.streams.values() {
            let (mut srtt, mut rttvar) = (o.srtt, o.rttvar);
            for at in &o.answered_sent {
                (srtt, rttvar) = sampled(srtt, rttvar, now.saturating_duration_since(*at));
            }
            let wait = rto(srtt, rttvar, self.patience);
            for p in &o.pending {
                let at = match (p.sent, p.wanted) {
                    (None, _) | (Some(_), Wanted::Now) => now,
                    (Some(sent), Wanted::No) => sent.checked_add(wait).unwrap_or(now),
                    (Some(sent), Wanted::Again) => {
                        let overdue = sent.checked_add(wait).unwrap_or(now);
                        p.went_for_want.and_then(|w| w.checked_add(WANT_AGAIN)).map_or(now, |again| again.min(overdue))
                    }
                };
                due = Some(due.map_or(at, |d| d.min(at)));
            }
        }
        due
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
                let mut said: BTreeMap<Arc<str>, Has> = all.into_iter().map(|h| (Arc::clone(&h.stream), h)).collect();
                let streams: Vec<String> = self.streams.keys().cloned().collect();
                // Only a link's first says the receiver lost what it had (it started again): one
                // said again may have been made before answers that reached here ahead of it.
                let first = !self.up;
                for stream in streams {
                    let has = match said.remove(stream.as_str()) {
                        Some(has) => has,
                        None => Has { stream: Arc::from(stream), upto: 0, nacks: Vec::new() },
                    };
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
        let Some(o) = self.streams.get_mut(&*has.stream) else { return events };
        if reconnect && has.upto < o.answered {
            events.push(Event::Regressed { stream: String::from(&*has.stream), has: has.upto, answered: o.answered });
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
                if !p.resent
                    && let Some(at) = p.sent
                {
                    o.answered_sent.push(at);
                }
            }
            events.push(Event::Answered { stream: String::from(&*has.stream), seq, outcome });
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

    /// Whether the link is up and the peer has said what it has.
    pub fn up(&self) -> bool {
        self.up
    }

    /// Whether this side sends `stream` (it has items of it, or had).
    pub fn sends(&self, stream: &str) -> bool {
        self.streams.contains_key(stream)
    }

    /// Through which item `stream` is answered (0: none yet).
    pub fn answered_through(&self, stream: &str) -> Seq {
        self.streams.get(stream).map_or(0, |o| o.answered)
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

struct In {
    /// Its stream's name, shared by every answer it makes ([`Has`]).
    name: Arc<str>,
    /// Every item through here is answered.
    upto: Seq,
    /// The item being applied now: a copy that comes meanwhile is dropped (its answer comes once
    /// it's applied).
    taking: Option<Seq>,
    /// Refusals after the sender's `low`: repeated in every answer until it has them.
    nacks: BTreeMap<Seq, (Refusal, String)>,
    /// The gap it asked for (`Want`), until it has it.
    wanted: Option<Asked>,
}

/// A gap a receiver asked for.
#[derive(Clone, Copy)]
struct Asked {
    /// The first item it lacks.
    from: Seq,
    /// When it last asked.
    at: Instant,
    /// How many items past the gap came since it first asked.
    past: u32,
    /// How many more times it asks on its own clock unless an item past the gap comes.
    left: u32,
}

/// Items past one gap a receiver takes in before it asks for the gap again.
const WANT_EVERY: u32 = 64;

/// How long a [`Receiver::timed`] waits after it asked for a gap before it asks again while the
/// gap is still there. Longer than [`WANT_AGAIN`], so its sender sends the gap again for each.
pub const ASK_AGAIN: Duration = Duration::from_millis(500);

/// How many times a [`Receiver::timed`] asks for a gap on its own clock ([`Receiver::wants`])
/// after the last item past it came: one whose sender let its stream go isn't asked for for ever.
const ASKS_ALONE: u32 = 8;

/// When a receiver asks again for a gap it asked for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Asking {
    /// Every [`WANT_EVERY`] items past it.
    #[default]
    Counted,
    /// Once [`ASK_AGAIN`] has passed, as items past it come and on its own clock.
    Timed,
}

impl In {
    /// Stream `name`'s, as its first item finds it: nothing answered yet.
    fn named(name: Arc<str>) -> Self {
        Self { name, upto: 0, taking: None, nacks: BTreeMap::new(), wanted: None }
    }

    /// [`Receiver::on_item`] of this stream, its receiver asking for gaps again as `asking` says.
    fn on_item(&mut self, seq: Seq, low: Seq, now: Instant, asking: Asking) -> Arrival {
        self.nacks.retain(|n, _| *n > low);
        if self.taking == Some(seq) {
            return Arrival::Answered(None);
        }
        // Its sender has every answer through `low`: what this side doesn't know of them (it
        // started again since, and the stream went on) isn't missing.
        if self.taking.is_none() && self.upto < low {
            self.upto = low;
        }
        if seq <= self.upto {
            return Arrival::Answered(Some(Frame::Ack(self.has())));
        }
        if seq == self.upto.saturating_add(1) && self.taking.is_none() {
            self.taking = Some(seq);
            self.wanted = None;
            return Arrival::Take(Arc::clone(&self.name));
        }
        let from = self.upto.saturating_add(1);
        match &mut self.wanted {
            Some(w) if w.from == from => {
                w.past = w.past.saturating_add(1);
                w.left = ASKS_ALONE;
                let again = match asking {
                    Asking::Counted => w.past.is_multiple_of(WANT_EVERY),
                    Asking::Timed => now.saturating_duration_since(w.at) >= ASK_AGAIN,
                };
                // Asked for not long ago: its sender is sending it again.
                if !again {
                    return Arrival::Answered(None);
                }
                w.at = now;
            }
            w => *w = Some(Asked { from, at: now, past: 0, left: ASKS_ALONE }),
        }
        Arrival::Gap(Frame::Want { stream: String::from(&*self.name), from })
    }

    /// [`Receiver::took`] or [`Receiver::refused`] of this stream.
    fn done(&mut self, seq: Seq, refusal: Option<(Refusal, String)>) -> Frame {
        if self.taking == Some(seq) {
            self.taking = None;
            self.upto = seq;
            if let Some(r) = refusal {
                self.nacks.insert(seq, r);
                while self.nacks.len() > NACKS_KEPT {
                    self.nacks.pop_first();
                }
            }
        }
        Frame::Ack(self.has())
    }

    fn has(&self) -> Has {
        Has {
            stream: Arc::clone(&self.name),
            upto: self.upto,
            nacks: self.nacks.iter().map(|(seq, (code, why))| Nack { seq: *seq, code: *code, why: why.clone() }).collect(),
        }
    }
}

/// What to do with an item that came.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arrival {
    /// Apply it, then say [`Receiver::took`] or [`Receiver::refused`]: its stream's name as its receiver keeps it, shared,
    /// for its answer once what it came in is gone (goal 9, plan 01a1a87b2cec step 3: the hub's loop copied each item's out
    /// of its frame to answer it, and the receiver copied it again into the answer).
    Take(Arc<str>),
    /// It was answered already: send this answer again. `None`: it's being applied, and its
    /// answer comes once it is.
    Answered(Option<Frame>),
    /// It's past a gap: send this `Want` back. The item is dropped, and comes again in order.
    /// (Not for each item past a gap: those that come soon after the first are dropped answered
    /// `Answered(None)`.)
    Gap(Frame),
}

/// The receiving side of the streams from one peer.
#[derive(Default)]
pub struct Receiver {
    streams: BTreeMap<Arc<str>, In>,
    asking: Asking,
}

impl Receiver {
    /// One that asks for a gap again every [`WANT_EVERY`] items past it.
    pub fn new() -> Self {
        Self::default()
    }

    /// One that asks for a gap again on time ([`ASK_AGAIN`]): for streams with few items in flight
    /// (a runner's of its server's commands), past whose gap fewer than [`WANT_EVERY`] ever come.
    /// Its caller sends [`Receiver::wants`] when [`Receiver::want_due`] says.
    pub fn timed() -> Self {
        Self { streams: BTreeMap::new(), asking: Asking::Timed }
    }

    /// An item of stream `stream` came, at `now`: whether to apply what it carries, which its caller holds. A stream's name
    /// is copied only as its first item makes it (goal 9, plan 01a1a87b2cec step 3: every item's was, the server's for each
    /// of a runner's).
    pub fn on_item(&mut self, stream: &str, seq: Seq, low: Seq, now: Instant) -> Arrival {
        let asking = self.asking;
        match self.streams.get_mut(stream) {
            Some(i) => i.on_item(seq, low, now, asking),
            None => self.added(stream).on_item(seq, low, now, asking),
        }
    }

    /// Stream `stream`'s, made as it first comes.
    fn added(&mut self, stream: &str) -> &mut In {
        let name: Arc<str> = Arc::from(stream);
        self.streams.entry(Arc::clone(&name)).or_insert_with(|| In::named(name))
    }

    /// What it has of stream `stream` that [`Receiver::has`] says, compared: through which item, and how many refusals; none
    /// when it has no such stream.
    pub fn seen(&self, stream: &str) -> Option<(Seq, usize)> {
        self.streams.get(stream).map(|i| (i.upto, i.nacks.len()))
    }

    /// Whether `nacks` are the refusals it keeps of stream `stream`, in order (none for a stream it doesn't have).
    pub fn refusals_are(&self, stream: &str, nacks: &[Nack]) -> bool {
        let kept = self.streams.get(stream).map(|i| &i.nacks);
        kept.map_or(0, BTreeMap::len) == nacks.len() && kept.into_iter().flatten().zip(nacks).all(|((seq, (code, why)), n)| *seq == n.seq && *code == n.code && *why == n.why)
    }

    /// A [`Receiver::timed`]'s gaps to ask for again now (send each `Want`): those still there
    /// [`ASK_AGAIN`] after they were last asked for, whether or not more items past them come. A
    /// few times after the last that did ([`ASKS_ALONE`]); one past it that comes later asks again.
    /// None for the others.
    pub fn wants(&mut self, now: Instant) -> Vec<Frame> {
        let mut out = Vec::new();
        if self.asking != Asking::Timed {
            return out;
        }
        for (stream, i) in &mut self.streams {
            let from = i.upto.saturating_add(1);
            // Filled from its sender's `low`.
            if i.wanted.is_some_and(|w| w.from != from) {
                i.wanted = None;
            }
            let Some(w) = i.wanted.as_mut() else { continue };
            if i.taking.is_none() && w.left > 0 && now.saturating_duration_since(w.at) >= ASK_AGAIN {
                w.at = now;
                w.left -= 1;
                out.push(Frame::Want { stream: String::from(&**stream), from });
            }
        }
        out
    }

    /// When [`Receiver::wants`] next has a gap to ask for again, if it will.
    pub fn want_due(&self) -> Option<Instant> {
        if self.asking != Asking::Timed {
            return None;
        }
        self.streams
            .values()
            .filter(|i| i.taking.is_none())
            .filter_map(|i| i.wanted.filter(|w| w.left > 0 && w.from == i.upto.saturating_add(1)))
            .filter_map(|w| w.at.checked_add(ASK_AGAIN))
            .min()
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
        match self.streams.get_mut(stream) {
            Some(i) => i.done(seq, refusal),
            None => self.added(stream).done(seq, refusal),
        }
    }

    /// What this side has of every stream it receives: the first frame once a link comes up.
    pub fn have(&self) -> Frame {
        Frame::Have(self.streams.values().map(In::has).collect())
    }

    /// What it has of stream `stream`, as [`Receiver::have`] says it of each; none when it has no such
    /// stream.
    pub fn has(&self, stream: &str) -> Option<Has> {
        self.streams.get(stream).map(In::has)
    }

    /// The link went down: an item being applied is answered when it's done, on the next link, and
    /// a gap is asked for again only once an item past it comes on that one (its sender sends again
    /// from what this side says it has).
    pub fn down(&mut self) {
        for i in self.streams.values_mut() {
            i.wanted = None;
        }
    }

    /// What a durable stream's receiver keeps (`crate::wire::keep`): through which item it has
    /// it, and its refusals its sender may not have heard.
    pub fn state(&self, stream: &str) -> Has {
        self.streams.get(stream).map_or_else(|| Has { stream: Arc::from(stream), upto: 0, nacks: Vec::new() }, In::has)
    }

    /// Picks a durable stream up after a restart, from what it kept.
    pub fn restore(&mut self, has: Has) {
        let i = self.streams.entry(Arc::clone(&has.stream)).or_insert_with(|| In::named(Arc::clone(&has.stream)));
        i.upto = has.upto;
        i.taking = None;
        i.nacks = has.nacks.into_iter().map(|n| (n.seq, (n.code, n.why))).collect();
    }

    /// Lets go of a stream that ended (a session closed, a sender's earlier incarnation).
    pub fn forget(&mut self, stream: &str) {
        self.streams.remove(stream);
    }

    /// Lets go of every stream whose name `drop` picks.
    pub fn forget_where(&mut self, mut drop: impl FnMut(&str) -> bool) {
        self.streams.retain(|s, _| !drop(s));
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

    /// A stream whose answers take long (a receiver behind) waits about as long again for each
    /// before it sends it again, not its patience: RFC 6298 §2's RTO from what its answers took.
    /// A slow receiver isn't sent again and again what it has yet to take.
    #[test]
    fn a_stream_whose_answers_take_long_waits_as_long_for_them() {
        let base = Instant::now();
        let mut tx = Sender::new(1 << 20, Duration::from_secs(10));
        let mut rx = Receiver::new();
        tx.on_frame(rx.have());
        let _ = tx.push("s", vec![1]);
        let first = tx.poll(base);
        // Its answer comes 30 s after it went (SRTT 30 s, RTTVAR 15 s: RTO 90 s).
        for a in deliver(&mut rx, first) {
            tx.on_frame(a);
        }
        let _ = tx.push("s", vec![2]);
        assert_eq!(items(&tx.poll(t(base, 30_000))), vec![2]);
        assert!(tx.poll(t(base, 40_001)).is_empty(), "not again at its patience");
        assert!(tx.poll(t(base, 119_999)).is_empty(), "nor before its answers' RTO");
        let again = tx.poll(t(base, 120_000));
        assert_eq!(items(&again), vec![2], "again once an answer that long is overdue");
        for a in deliver(&mut rx, again) {
            tx.on_frame(a);
        }
        assert_eq!(tx.unanswered(), 0);
    }

    /// A sender's frames, applied by a receiver that takes everything: its answers.
    fn deliver(r: &mut Receiver, frames: Vec<Frame>) -> Vec<Frame> {
        let mut back = Vec::new();
        for f in frames {
            if let Frame::Item { stream, seq, low, .. } = f {
                match r.on_item(&stream, seq, low, Instant::now()) {
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
    fn a_copy_is_taken_once_and_answered_again() {
        let now = Instant::now();
        let mut r = Receiver::new();
        assert_eq!(r.on_item("a", 1, 0, now), Arrival::Take("a".into()));
        // A copy while it's being applied: dropped; its answer comes once it's applied.
        assert_eq!(r.on_item("a", 1, 0, now), Arrival::Answered(None));
        let first = r.took("a", 1);
        // A copy after: the same answer again, nothing applied.
        assert_eq!(r.on_item("a", 1, 0, now), Arrival::Answered(Some(first)));
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
        let (Some(Frame::Item { seq: s1, low: l1, .. }), Some(Frame::Item { seq: s2, low: l2, .. })) = (sent.next(), sent.next()) else {
            anyhow::bail!("two items");
        };
        assert!(matches!(r.on_item("a", s1, l1, base), Arrival::Take(_)));
        let _lost = r.refused("a", s1, Refusal::Unknown, "no such approval");
        assert!(matches!(r.on_item("a", s2, l2, base), Arrival::Take(_)));
        // The refusal's own answer was lost: the next answer still carries it.
        let events = s.on_frame(r.took("a", s2));
        assert_eq!(
            events,
            vec![
                Event::Answered { stream: "a".into(), seq: 1, outcome: Outcome::Refused { code: Refusal::Unknown, why: "no such approval".into() } },
                Event::Answered { stream: "a".into(), seq: 2, outcome: Outcome::Taken },
            ]
        );
        Ok(())
    }

    #[test]
    fn a_gap_asks_for_what_is_missing() {
        let base = Instant::now();
        let mut r = Receiver::new();
        assert_eq!(r.on_item("a", 2, 0, base), Arrival::Gap(Frame::Want { stream: "a".into(), from: 1 }));
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
        let base = Instant::now();
        let mut r = Receiver::new();
        assert!(matches!(r.on_item("a", 1, 0, base), Arrival::Take(_)));
        r.took("a", 1);
        let want = |from| Arrival::Gap(Frame::Want { stream: "a".into(), from });
        assert_eq!(r.on_item("a", 3, 0, base), want(2));
        assert_eq!(r.on_item("a", 4, 0, base), Arrival::Answered(None), "asked for once");
        // Asked again only after many more past it (that want lost below the channel), however
        // long that takes.
        let wants = (5..200).filter(|n| matches!(r.on_item("a", *n, 0, t(base, 10_000)), Arrival::Gap(_))).count();
        assert_eq!(wants, 3, "once every 64 past the gap");
        assert!(matches!(r.on_item("a", 2, 0, base), Arrival::Take(_)));
        r.took("a", 2);
        // A gap further on is asked for in its turn.
        assert_eq!(r.on_item("a", 5, 0, base), want(3));

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
        let now = Instant::now();
        let mut r = Receiver::new();
        assert!(matches!(r.on_item("p:7.1", 41, 40, now), Arrival::Take(_)));
        assert_eq!(r.took("p:7.1", 41), Frame::Ack(Has { stream: "p:7.1".into(), upto: 41, nacks: Vec::new() }));
        // One answered already, by the one before it: answered so again.
        assert!(matches!(r.on_item("p:7.1", 39, 40, now), Arrival::Answered(Some(_))));
    }

    /// A timed receiver (a runner's of its server's commands, few in flight) asks for a gap once,
    /// and again once ASK_AGAIN has passed, not for each item past it.
    #[test]
    fn a_timed_receiver_asks_for_a_gap_again_once_ask_again_has_passed() {
        let base = Instant::now();
        let mut r = Receiver::timed();
        assert!(matches!(r.on_item("a", 1, 0, base), Arrival::Take(_)));
        r.took("a", 1);
        let want = |from| Arrival::Gap(Frame::Want { stream: "a".into(), from });
        assert_eq!(r.on_item("a", 3, 0, base), want(2));
        assert_eq!(r.on_item("a", 4, 0, t(base, 1)), Arrival::Answered(None), "asked for once");
        assert!((5..200).all(|n| r.on_item("a", n, 0, t(base, 2)) == Arrival::Answered(None)), "however many come past it");
        // That want lost below the channel, or what it brought: asked again once ASK_AGAIN has passed.
        assert_eq!(r.on_item("a", 3, 0, t(base, 499)), Arrival::Answered(None));
        assert_eq!(r.on_item("a", 3, 0, t(base, 500)), want(2));
        assert!(matches!(r.on_item("a", 2, 0, t(base, 501)), Arrival::Take(_)));
        r.took("a", 2);
        assert_eq!(r.on_item("a", 5, 0, t(base, 502)), want(3));
    }

    /// A timed receiver's gap whose want, or what it brought, was lost is asked for again on its
    /// own clock, whether or not more items past it come: a few times after the last that did, and
    /// not once it's filled or the link went down. One that counts has no clock of its own.
    #[test]
    fn a_gap_is_asked_for_again_on_the_receivers_own_clock() {
        let base = Instant::now();
        let mut counted = Receiver::new();
        assert!(matches!(counted.on_item("a", 2, 0, base), Arrival::Gap(_)));
        assert!(counted.want_due().is_none() && counted.wants(t(base, 60_000)).is_empty());
        let mut r = Receiver::timed();
        assert!(r.wants(base).is_empty() && r.want_due().is_none());
        assert!(matches!(r.on_item("a", 2, 0, base), Arrival::Gap(_)));
        assert_eq!(r.want_due(), Some(t(base, 500)));
        assert!(r.wants(t(base, 499)).is_empty(), "not before ASK_AGAIN");
        let want = vec![Frame::Want { stream: "a".into(), from: 1 }];
        assert_eq!(r.wants(t(base, 500)), want);
        assert_eq!(r.want_due(), Some(t(base, 1_000)));
        // Nothing more came past it: its sender may have let the stream go.
        let asked = (2..40).filter(|n| !r.wants(t(base, 500 * n)).is_empty()).count();
        assert_eq!(asked, 7, "ASKS_ALONE in all");
        assert_eq!(r.want_due(), None);
        // One that comes past it asks again, and so does the clock after it.
        assert!(matches!(r.on_item("a", 3, 0, t(base, 20_000)), Arrival::Gap(_)));
        assert_eq!(r.wants(t(base, 20_500)), want);
        // Filled: nothing to ask for.
        assert!(matches!(r.on_item("a", 1, 0, t(base, 20_600)), Arrival::Take(_)));
        r.took("a", 1);
        assert!(r.wants(t(base, 30_000)).is_empty() && r.want_due().is_none());
        // The link went down: the next asks once an item past the gap comes on it.
        assert!(matches!(r.on_item("a", 3, 0, t(base, 30_000)), Arrival::Gap(_)));
        r.down();
        assert!(r.want_due().is_none() && r.wants(t(base, 40_000)).is_empty());
        assert!(matches!(r.on_item("a", 3, 0, t(base, 40_001)), Arrival::Gap(_)));
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
            // Half the schedules a receiver that asks for a gap again on time, as a runner's of
            // its server's commands does.
            let receiver = || if seed.is_multiple_of(2) { Receiver::new() } else { Receiver::timed() };
            let mut r = receiver();
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
                    let mut again = receiver();
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
                    let answer = match r.on_item(&stream, seq, low, now) {
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
                // The gaps it still lacks, asked for again on its own clock.
                if up {
                    for w in r.wants(now) {
                        if !(faults && rng.one_in(12)) {
                            to_s.push((tick + if faults { rng.below(6) } else { 0 }, w));
                        }
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
