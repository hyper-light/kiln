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
//!   ([`Full`]), and nothing is shed.

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

struct Pending {
    seq: Seq,
    body: Vec<u8>,
    /// When it was last sent since the link came up; `None`: not yet.
    sent: Option<Instant>,
}

struct Out {
    /// The number the next item gets.
    next: Seq,
    /// Every item through here is answered.
    answered: Seq,
    pending: VecDeque<Pending>,
    bytes: usize,
}

impl Default for Out {
    fn default() -> Self {
        Self { next: 1, answered: 0, pending: VecDeque::new(), bytes: 0 }
    }
}

/// The sending side of the streams to one peer.
pub struct Sender {
    streams: BTreeMap<String, Out>,
    /// The most bytes a stream keeps unanswered (an item larger than that goes alone).
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
    /// keeps as much unanswered as it may.
    pub fn push(&mut self, stream: &str, body: Vec<u8>) -> Result<Seq, Full> {
        let o = self.streams.entry(stream.to_owned()).or_default();
        if !o.pending.is_empty() && o.bytes.saturating_add(body.len()) > self.room {
            return Err(Full);
        }
        let seq = o.next;
        o.next = o.next.saturating_add(1);
        o.bytes = o.bytes.saturating_add(body.len());
        o.pending.push_back(Pending { seq, body, sent: None });
        Ok(seq)
    }

    /// Picks a durable stream up after a restart: the number its next item gets, and what it
    /// had sent and not had answered (`crate::wire::keep`), in order.
    pub fn restore(&mut self, stream: &str, next: Seq, pending: Vec<(Seq, Vec<u8>)>) {
        let o = self.streams.entry(stream.to_owned()).or_default();
        o.pending = pending.into_iter().map(|(seq, body)| Pending { seq, body, sent: None }).collect();
        o.bytes = o.pending.iter().map(|p| p.body.len()).sum();
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
                let due = p.sent.is_none_or(|at| now.saturating_duration_since(at) >= self.patience);
                if due {
                    p.sent = Some(now);
                    out.push(Frame::Item { stream: stream.clone(), seq: p.seq, low, body: p.body.clone() });
                }
            }
        }
        out
    }

    /// The link went down: what was sent on it goes again once the peer says what it has.
    pub fn down(&mut self) {
        self.up = false;
        for o in self.streams.values_mut() {
            for p in &mut o.pending {
                p.sent = None;
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
                            p.sent = None;
                        }
                    }
                }
                self.up = true;
                events
            }
            Frame::Want { stream, from } => {
                if let Some(o) = self.streams.get_mut(&stream) {
                    for p in o.pending.iter_mut().filter(|p| p.seq >= from) {
                        p.sent = None;
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
}

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
    Gap(Frame),
}

/// The receiving side of the streams from one peer.
#[derive(Default)]
pub struct Receiver {
    streams: BTreeMap<String, In>,
}

impl Receiver {
    pub fn new() -> Self {
        Self::default()
    }

    /// An item came.
    pub fn on_item(&mut self, stream: &str, seq: Seq, low: Seq, body: Vec<u8>) -> Arrival {
        let i = self.streams.entry(stream.to_owned()).or_default();
        i.nacks.retain(|n, _| *n > low);
        if i.taking == Some(seq) {
            return Arrival::Answered(None);
        }
        if seq <= i.upto {
            return Arrival::Answered(Some(Frame::Ack(i.has(stream))));
        }
        if seq == i.upto.saturating_add(1) && i.taking.is_none() {
            i.taking = Some(seq);
            return Arrival::Take(body);
        }
        Arrival::Gap(Frame::Want { stream: stream.to_owned(), from: i.upto.saturating_add(1) })
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
        let i = self.streams.entry(has.stream.clone()).or_default();
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
