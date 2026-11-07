//! The server's end of each runner's channel (docs/wire.md): what it has of every stream a
//! runner sends it, and the answers it gives.
//!
//! An item is answered once the server has applied what it carries, never on arrival. What the
//! server has is in memory, so a restarted server has nothing of any stream, and the runner
//! starts each one over, whole, once (`crate::wire::uplink`).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use parking_lot::Mutex;

use super::channel::{Arrival, Frame, Has, Nack, Receiver, Refusal, Seq};
use super::uplink::key_of;
use crate::rcu::Rcu;

/// One runner's streams as its receivers have them ([`Receiver::have`]), by name (the order its
/// receivers keep them in), each stream's published as its loop changes it ([`Seen`]).
type Streams = Rcu<BTreeMap<String, Arc<Seen>>>;

/// What a runner's receivers have of one stream, as its loop last published it: through which item, and the refusals its
/// sender may not have heard, swapped in only as they change. Its loop stores the refusals before the item, and a reader
/// loads the item before the refusals, so what's read says every refusal through that item (one past it only says more,
/// which its sender takes up once the item it's for is said). Goal 9, plan 01a1a87b2cec step 3: a whole `Has` was made and
/// swapped in for every answer, its stream's name copied each time, for readers that look every few seconds.
#[derive(Default)]
struct Seen {
    upto: AtomicU64,
    nacks: arc_swap::ArcSwap<Vec<Nack>>,
}

impl Seen {
    /// What it says of stream `stream`, as [`Receiver::has`] says it.
    fn has(&self, stream: &str) -> Has {
        let upto = self.upto.load(Ordering::Acquire);
        Has { stream: Arc::from(stream), upto, nacks: Vec::clone(&self.nacks.load()) }
    }
}

/// What the server has of each runner's streams, by runner name: one hub loop's (`crate::wire::fan`; goal 10,
/// plan 01a1a87b2dae step 4), the control loop's for the streams it takes (terminals') and each shard's session
/// loop's for its sessions' (`Hub::links_of`). "Its loop" below is that one.
#[derive(Default)]
pub struct Links {
    /// Each runner's receivers: its loop alone takes these, for each item a runner sends and then
    /// its answer, in order; no other task does (what's said again of them is read off `has`).
    by_runner: Mutex<HashMap<String, Receiver>>,
    /// What each runner's receivers have of each of its streams, as [`Receiver::have`] says it: its
    /// loop swaps a stream's in as it changes it, and what's said again every few seconds (by the hub's
    /// commands loop) is read off them without a lock (goal 5, plan 01a1a59c2aae step 2).
    has: Rcu<HashMap<String, Arc<Streams>>>,
    /// Runners that said they keep the channel: what they send comes again after a cut, so their
    /// terminals stay open through it. A copy read without a lock, swapped in as one says otherwise
    /// (goal 5, plan 01a1a59c2aae step 2: `crate::rcu`).
    keep: Rcu<std::collections::HashSet<String>>,
}

impl Links {
    pub fn new() -> Self {
        Self::default()
    }

    /// What the server has of `runner`'s streams: sent when its link comes up, and again every few
    /// seconds. Read off the copies its loop keeps of its receivers, as they have them.
    pub fn have(&self, runner: &str) -> Frame {
        let streams = self.has.load().get(runner).cloned();
        Frame::Have(streams.map(|s| s.load().iter().map(|(name, seen)| seen.has(name)).collect()).unwrap_or_default())
    }

    /// An item of stream `stream` came from `runner`, at `now`: whether to apply what it carries, which its caller holds
    /// ([`Receiver::on_item`]). A stream's first item starts its key over: the server lets go of what it had of that
    /// session's (or terminal's) earlier streams.
    pub fn on_item(&self, runner: &str, stream: &str, seq: Seq, low: Seq, now: Instant) -> Arrival {
        self.with_receiver(runner, |rx| {
            let mut gone: Vec<String> = Vec::new();
            if seq == 1
                && let Some(key) = key_of(stream)
            {
                let prefix = format!("{key}#");
                rx.forget_where(|s| {
                    let goes = s != stream && s.starts_with(&prefix);
                    if goes {
                        gone.push(s.to_owned());
                    }
                    goes
                });
            }
            // What its receivers say of the stream changes as an item comes only where it's the stream's first, or its
            // sender's `low` let go of refusals or moved past what they had: else nothing is published (goal 9, plan
            // 01a1a87b2cec step 3: every item's was, made anew).
            let seen = rx.seen(stream);
            let arrival = rx.on_item(stream, seq, low, now);
            if !gone.is_empty() || rx.seen(stream) != seen {
                self.publish(runner, stream, rx, &gone);
            }
            arrival
        })
    }

    /// The item being applied from `runner` was taken (`None`) or refused: the answer to send it.
    pub fn answer(&self, runner: &str, stream: &str, seq: Seq, refusal: Option<(Refusal, String)>) -> Frame {
        self.with_receiver(runner, |rx| {
            let frame = match refusal {
                None => rx.took(stream, seq),
                Some((code, why)) => rx.refused(stream, seq, code, why),
            };
            self.publish(runner, stream, rx, &[]);
            frame
        })
    }

    /// `f` of `runner`'s receivers, under their lock (its loop's alone). Its name is copied only as its first item makes them
    /// (goal 9, plan 01a1a87b2cec step 3: copied for each item and each answer).
    fn with_receiver<R>(&self, runner: &str, f: impl FnOnce(&mut Receiver) -> R) -> R {
        let mut g = self.by_runner.lock();
        match g.get_mut(runner) {
            Some(rx) => f(rx),
            None => f(g.entry(runner.to_owned()).or_default()),
        }
    }

    /// Stream `stream` of `runner` as its receivers `rx` have it now (none: they have it no more), and the streams they let
    /// go of (`gone`): into the copies what's said again is read off. Its loop publishes, as it changes them, so the copies
    /// are what the receivers have between its changes.
    fn publish(&self, runner: &str, stream: &str, rx: &Receiver, gone: &[String]) {
        let streams = self.streams_of(runner);
        if !gone.is_empty() {
            streams.update(|m| {
                for s in gone {
                    m.remove(s);
                }
            });
        }
        let Some((upto, _)) = rx.seen(stream) else {
            if streams.load().contains_key(stream) {
                streams.update(|m| m.remove(stream));
            }
            return;
        };
        let held = streams.load().get(stream).cloned();
        let seen = held.unwrap_or_else(|| {
            let seen = Arc::new(Seen::default());
            streams.update(|m| m.insert(stream.to_owned(), Arc::clone(&seen)));
            seen
        });
        if !rx.refusals_are(stream, &seen.nacks.load()) {
            seen.nacks.store(Arc::new(rx.has(stream).map(|h| h.nacks).unwrap_or_default()));
        }
        seen.upto.store(upto, Ordering::Release);
    }

    /// `runner`'s streams' copies: the ones there, else new ones swapped in with them.
    fn streams_of(&self, runner: &str) -> Arc<Streams> {
        if let Some(s) = self.has.load().get(runner) {
            return Arc::clone(s);
        }
        self.has.update(|m| Arc::clone(m.entry(runner.to_owned()).or_default()))
    }

    /// `runner` is gone for good (removed): what the server had of its streams goes too.
    pub fn forget(&self, runner: &str) {
        self.by_runner.lock().remove(runner);
        self.has.update(|m| m.remove(runner));
        self.keep.update(|k| k.remove(runner));
    }

    /// Whether `runner` keeps the channel, as it last said (`AgentMsg::Caps`).
    pub fn set_keeps(&self, runner: &str, keeps: bool) {
        // Said again as it was, nothing changes: a copy swapped in only for a change.
        if self.keep.load().contains(runner) == keeps {
            return;
        }
        self.keep.update(|k| {
            if keeps {
                k.insert(runner.to_owned());
            } else {
                k.remove(runner);
            }
        });
    }

    /// Whether `runner` keeps the channel: what it sends comes again after a cut.
    pub fn keeps(&self, runner: &str) -> bool {
        self.keep.load().contains(runner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal 5, plan 01a1a59c2aae step 2: what's said again of a runner's streams (`have`) is read off the
    /// copies its loop publishes as it takes items and answers them, and is what its receivers have,
    /// stream by stream and in their order, after items taken, refused and started over.
    #[test]
    fn g5_what_is_said_again_is_what_the_receivers_have() {
        let now = Instant::now();
        let links = Links::new();
        let mut receivers = Receiver::new();
        let steps: [(&str, Seq, Option<(Refusal, String)>); 6] = [
            ("s:x#b1.0", 1, None),
            ("s:x#b1.0", 2, Some((Refusal::Refused, "no".into()))),
            ("t:term#b1.0", 1, None),
            ("s:x#b1.1", 1, None),
            ("s:y#b1.0", 1, Some((Refusal::Malformed, "cut short".into()))),
            ("t:term#b1.0", 2, None),
        ];
        for (stream, seq, refusal) in steps {
            let _ = links.on_item("a", stream, seq, 0, now);
            let _ = links.answer("a", stream, seq, refusal.clone());
            if seq == 1
                && let Some(key) = key_of(stream)
            {
                let prefix = format!("{key}#");
                receivers.forget_where(|s| s != stream && s.starts_with(&prefix));
            }
            let _ = receivers.on_item(stream, seq, 0, now);
            let _ = match refusal {
                None => receivers.took(stream, seq),
                Some((code, why)) => receivers.refused(stream, seq, code, why),
            };
            let (Frame::Have(said), Frame::Have(has)) = (links.have("a"), receivers.have()) else { return };
            assert_eq!(said, has, "after {stream} {seq}");
        }
        let Frame::Have(none) = links.have("b") else { return };
        assert!(none.is_empty());
    }

    #[test]
    fn a_sessions_new_stream_replaces_its_old_one() {
        let now = Instant::now();
        let links = Links::new();
        assert!(matches!(links.on_item("a", "s:x#b1.0", 1, 0, now), Arrival::Take(_)));
        links.answer("a", "s:x#b1.0", 1, None);
        assert!(matches!(links.on_item("a", "s:y#b1.0", 1, 0, now), Arrival::Take(_)));
        links.answer("a", "s:y#b1.0", 1, None);
        // x starts over under its next generation: its old stream goes; y's stays.
        assert!(matches!(links.on_item("a", "s:x#b1.1", 1, 0, now), Arrival::Take(_)));
        links.answer("a", "s:x#b1.1", 1, Some((Refusal::Refused, "no".into())));
        let Frame::Have(has) = links.have("a") else { return };
        let mut names: Vec<&str> = has.iter().map(|h| &*h.stream).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["s:x#b1.1", "s:y#b1.0"]);
        // Another runner's are its own.
        let Frame::Have(other) = links.have("b") else { return };
        assert!(other.is_empty());
    }
    /// Goal 9, plan 01a1a87b2cec step 3: once a runner's stream is known, an item that changes nothing its receivers say, and
    /// its answer, cost them nothing made: what they publish of the stream is stored in place, and the answer's stream name is
    /// the one they keep, shared. The runner's and the stream's names were copied for each item and each answer, and a whole
    /// copy of what the receivers have made and swapped in for each item and each answer, though an item had changed nothing
    /// it said: nine blocks an item; then one, its answer's copy of its stream's name. What's said again is still what the
    /// receivers have.
    #[test]
    fn g9_s3_an_item_and_its_answer_make_nothing() {
        let now = Instant::now();
        let links = Links::new();
        let mut receivers = Receiver::new();
        for seq in 1..=8 {
            let before = crate::os::counted::made();
            let taken = links.on_item("a", "s:x#b1.0", seq, seq - 1, now);
            let item = crate::os::counted::made();
            let answer = links.answer("a", "s:x#b1.0", seq, None);
            let answered = crate::os::counted::made();
            assert!(matches!(taken, Arrival::Take(_)), "item {seq} not taken");
            assert!(matches!(&answer, Frame::Ack(h) if h.upto == seq), "item {seq} not answered taken");
            // The first two make the runner's and the stream's places.
            if seq > 2 {
                assert_eq!(item.blocks - before.blocks, 0, "item {seq}: nothing made for it");
                assert_eq!(answered.blocks - item.blocks, 0, "item {seq}'s answer: nothing made for it");
            }
            let _ = receivers.on_item("s:x#b1.0", seq, seq - 1, now);
            let _ = receivers.took("s:x#b1.0", seq);
            let (Frame::Have(said), Frame::Have(has)) = (links.have("a"), receivers.have()) else { return };
            assert_eq!(said, has, "after item {seq}");
        }
    }
}
