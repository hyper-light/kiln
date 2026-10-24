//! The server's end of each runner's channel (docs/wire.md): what it has of every stream a
//! runner sends it, and the answers it gives.
//!
//! An item is answered once the server has applied what it carries, never on arrival. What the
//! server has is in memory, so a restarted server has nothing of any stream, and the runner
//! starts each one over, whole, once (`crate::wire::uplink`).

use std::collections::HashMap;

use parking_lot::Mutex;

use super::channel::{Arrival, Frame, Receiver, Refusal, Seq};
use super::uplink::key_of;

/// What the server has of each runner's streams, by runner name.
#[derive(Default)]
pub struct Links {
    by_runner: Mutex<HashMap<String, Receiver>>,
    /// Runners that said they keep the channel: what they send comes again after a cut, so their
    /// terminals stay open through it.
    keep: Mutex<std::collections::HashSet<String>>,
}

impl Links {
    pub fn new() -> Self {
        Self::default()
    }

    /// What the server has of `runner`'s streams: sent when its link comes up.
    pub fn have(&self, runner: &str) -> Frame {
        self.by_runner.lock().entry(runner.to_owned()).or_default().have()
    }

    /// An item came from `runner`. A stream's first item starts its key over: the server lets go
    /// of what it had of that session's (or terminal's) earlier streams.
    pub fn on_item(&self, runner: &str, stream: &str, seq: Seq, low: Seq, body: Vec<u8>) -> Arrival {
        let mut g = self.by_runner.lock();
        let rx = g.entry(runner.to_owned()).or_default();
        if seq == 1
            && let Some(key) = key_of(stream)
        {
            let prefix = format!("{key}#");
            rx.forget_where(|s| s != stream && s.starts_with(&prefix));
        }
        rx.on_item(stream, seq, low, body)
    }

    /// The item being applied from `runner` was taken (`None`) or refused: the answer to send it.
    pub fn answer(&self, runner: &str, stream: &str, seq: Seq, refusal: Option<(Refusal, String)>) -> Frame {
        let mut g = self.by_runner.lock();
        let rx = g.entry(runner.to_owned()).or_default();
        match refusal {
            None => rx.took(stream, seq),
            Some((code, why)) => rx.refused(stream, seq, code, why),
        }
    }

    /// `runner` is gone for good (removed): what the server had of its streams goes too.
    pub fn forget(&self, runner: &str) {
        self.by_runner.lock().remove(runner);
        self.keep.lock().remove(runner);
    }

    /// Whether `runner` keeps the channel, as it last said (`AgentMsg::Caps`).
    pub fn set_keeps(&self, runner: &str, keeps: bool) {
        let mut k = self.keep.lock();
        if keeps {
            k.insert(runner.to_owned());
        } else {
            k.remove(runner);
        }
    }

    /// Whether `runner` keeps the channel: what it sends comes again after a cut.
    pub fn keeps(&self, runner: &str) -> bool {
        self.keep.lock().contains(runner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sessions_new_stream_replaces_its_old_one() {
        let links = Links::new();
        assert!(matches!(links.on_item("a", "s:x#b1.0", 1, 0, vec![1]), Arrival::Take(_)));
        links.answer("a", "s:x#b1.0", 1, None);
        assert!(matches!(links.on_item("a", "s:y#b1.0", 1, 0, vec![1]), Arrival::Take(_)));
        links.answer("a", "s:y#b1.0", 1, None);
        // x starts over under its next generation: its old stream goes; y's stays.
        assert!(matches!(links.on_item("a", "s:x#b1.1", 1, 0, vec![2]), Arrival::Take(_)));
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
