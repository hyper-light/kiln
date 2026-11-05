//! The server's end of each runner's channel (docs/wire.md): what it has of every stream a
//! runner sends it, and the answers it gives.
//!
//! An item is answered once the server has applied what it carries, never on arrival. What the
//! server has is in memory, so a restarted server has nothing of any stream, and the runner
//! starts each one over, whole, once (`crate::wire::uplink`).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;

use super::channel::{Arrival, Frame, Has, Receiver, Refusal, Seq};
use super::uplink::key_of;
use crate::rcu::Rcu;

/// One runner's streams as its receivers have them ([`Receiver::have`]), by name (the order its
/// receivers keep them in), each stream's swapped in as the hub's loop changes it.
type Streams = Rcu<BTreeMap<String, Arc<arc_swap::ArcSwap<Has>>>>;

/// What the server has of each runner's streams, by runner name.
#[derive(Default)]
pub struct Links {
    /// Each runner's receivers: the hub's loop alone takes these, for each item a runner sends and then
    /// its answer, in order; no other task does (what's said again of them is read off `has`).
    by_runner: Mutex<HashMap<String, Receiver>>,
    /// What each runner's receivers have of each of its streams, as [`Receiver::have`] says it: the hub's
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
    /// seconds. Read off the copies the hub's loop keeps of its receivers, as they have them.
    pub fn have(&self, runner: &str) -> Frame {
        let streams = self.has.load().get(runner).cloned();
        Frame::Have(streams.map(|s| s.load().values().map(|c| Has::clone(&c.load())).collect()).unwrap_or_default())
    }

    /// An item came from `runner`, at `now`. A stream's first item starts its key over: the server
    /// lets go of what it had of that session's (or terminal's) earlier streams.
    pub fn on_item(&self, runner: &str, stream: &str, seq: Seq, low: Seq, body: Vec<u8>, now: Instant) -> Arrival {
        let mut g = self.by_runner.lock();
        let rx = g.entry(runner.to_owned()).or_default();
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
        let arrival = rx.on_item(stream, seq, low, body, now);
        self.publish(runner, stream, rx.has(stream), &gone);
        arrival
    }

    /// The item being applied from `runner` was taken (`None`) or refused: the answer to send it.
    pub fn answer(&self, runner: &str, stream: &str, seq: Seq, refusal: Option<(Refusal, String)>) -> Frame {
        let mut g = self.by_runner.lock();
        let rx = g.entry(runner.to_owned()).or_default();
        let frame = match refusal {
            None => rx.took(stream, seq),
            Some((code, why)) => rx.refused(stream, seq, code, why),
        };
        self.publish(runner, stream, rx.has(stream), &[]);
        frame
    }

    /// Stream `stream` of `runner` as its receivers have it now (`has`; none: they have it no more), and
    /// the streams they let go of (`gone`): into the copies what's said again is read off. The hub's loop
    /// publishes, as it changes them, so the copies are what the receivers have between its changes.
    fn publish(&self, runner: &str, stream: &str, has: Option<Has>, gone: &[String]) {
        let streams = self.streams_of(runner);
        if !gone.is_empty() {
            streams.update(|m| {
                for s in gone {
                    m.remove(s);
                }
            });
        }
        let Some(has) = has else {
            if streams.load().contains_key(stream) {
                streams.update(|m| m.remove(stream));
            }
            return;
        };
        if let Some(cell) = streams.load().get(stream) {
            cell.store(Arc::new(has));
            return;
        }
        let cell = Arc::new(arc_swap::ArcSwap::from_pointee(has));
        streams.update(|m| m.insert(stream.to_owned(), Arc::clone(&cell)));
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
    /// copies the hub's loop publishes as it takes items and answers them, and is what its receivers have,
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
            let _ = links.on_item("a", stream, seq, 0, vec![1], now);
            let _ = links.answer("a", stream, seq, refusal.clone());
            if seq == 1
                && let Some(key) = key_of(stream)
            {
                let prefix = format!("{key}#");
                receivers.forget_where(|s| s != stream && s.starts_with(&prefix));
            }
            let _ = receivers.on_item(stream, seq, 0, vec![1], now);
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
        assert!(matches!(links.on_item("a", "s:x#b1.0", 1, 0, vec![1], now), Arrival::Take(_)));
        links.answer("a", "s:x#b1.0", 1, None);
        assert!(matches!(links.on_item("a", "s:y#b1.0", 1, 0, vec![1], now), Arrival::Take(_)));
        links.answer("a", "s:y#b1.0", 1, None);
        // x starts over under its next generation: its old stream goes; y's stays.
        assert!(matches!(links.on_item("a", "s:x#b1.1", 1, 0, vec![2], now), Arrival::Take(_)));
        links.answer("a", "s:x#b1.1", 1, Some((Refusal::Refused, "no".into())));
        let Frame::Have(has) = links.have("a") else { return };
        let mut names: Vec<&str> = has.iter().map(|h| h.stream.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["s:x#b1.1", "s:y#b1.0"]);
        // Another runner's are its own.
        let Frame::Have(other) = links.have("b") else { return };
        assert!(other.is_empty());
    }
}
