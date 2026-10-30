//! The server's part in reach (`docs/reach.md` §3.2): it carries a link's frames between the two
//! runners and nothing else. It sees ciphertext and who talks to whom; it never holds a key.
//!
//! A link is opened by its first frame (`open`), which names the runner it is for; the server
//! notes the two ends (each by name and connection) and from then on accepts frames for that link
//! only from those two connections, stamping each with the sender's name as its certificate gives
//! it. When either end's connection goes, the other end is told the link is closed.

use std::collections::HashMap;
use std::time::Instant;

use hyper_transport::PeerId;
use serde::{Deserialize, Serialize};

use crate::proto::ServerMsg;

/// Links one runner may have open through the server at once.
pub const LINKS_PER_RUNNER: usize = 256;

/// What a runner announces about itself for reach ([`crate::proto::AgentMsg::ReachHello`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Announce {
    /// Its identity's fingerprint ([`super::e2e::fingerprint_of_spki`]).
    pub fingerprint: String,
    /// It takes links from other runners (its owner can turn that off).
    #[serde(default)]
    pub accept: bool,
    /// Its inner protocol version ([`super::wire::VERSION`]).
    #[serde(default)]
    pub version: u32,
}

#[derive(Debug, Clone)]
struct Ends {
    a: String,
    a_peer: PeerId,
    b: String,
    b_peer: PeerId,
    #[allow(dead_code)]
    since: Instant,
}

/// Why the relay refused a runner's frame, from a closed set: told beside its words to a runner
/// that says `CAP_UNREAD` (`ServerMsg::ReachRefused`), and only in words to one from before
/// (`ServerMsg::Reach`'s `close`). It travels in frames: new ones are appended last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    /// Its link id is another link's.
    Taken,
    /// A runner doesn't open a link to itself.
    ToItself,
    /// Its runner has as many links open as one may ([`LINKS_PER_RUNNER`]).
    TooMany,
    /// The runner it's for doesn't take reach links (it never said it does: one from before).
    NotOffered,
    /// The runner it's for doesn't accept links from other runners (its owner turned that off).
    NotAccepted,
    /// The runner it's for isn't connected.
    NotConnected,
    /// Its link is closed.
    Closed,
}

impl Refusal {
    pub const ALL: [Refusal; 7] = [Refusal::Taken, Refusal::ToItself, Refusal::TooMany, Refusal::NotOffered, Refusal::NotAccepted, Refusal::NotConnected, Refusal::Closed];

    pub fn name(self) -> &'static str {
        match self {
            Refusal::Taken => "taken",
            Refusal::ToItself => "to_itself",
            Refusal::TooMany => "too_many",
            Refusal::NotOffered => "not_offered",
            Refusal::NotAccepted => "not_accepted",
            Refusal::NotConnected => "not_connected",
            Refusal::Closed => "closed",
        }
    }
}

/// What became of a runner's frame ([`Relay::forward`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Forwarded {
    /// What to send where: the frame to its link's other end; or nothing, for a goodbye on a
    /// link already gone.
    Sent(Vec<(PeerId, ServerMsg)>),
    /// Refused, back to its sender: about the link it named, to runner `to`, why in a code and
    /// in words.
    Refused { to: String, link: String, code: Refusal, why: String },
    /// From a runner that isn't an end of the link it names: nothing goes on. It's noted
    /// (`crate::wire::unread`), naming the link's `ends`.
    NotItsLink { link: String, ends: (String, String) },
}

impl Forwarded {
    /// What to send where, a refusal told to its sender (on connection `sender`): with its code
    /// when it says `CAP_UNREAD` (`typed`), in words alone when it's from before.
    pub fn sends(self, sender: PeerId, typed: bool) -> Vec<(PeerId, ServerMsg)> {
        match self {
            Forwarded::Sent(out) => out,
            Forwarded::Refused { to, link, code, why } if typed => vec![(sender, ServerMsg::ReachRefused { from: to, link, code, why })],
            Forwarded::Refused { to, link, why, .. } => vec![(sender, ServerMsg::Reach { from: to, link, open: false, data: Vec::new(), close: Some(why) })],
            Forwarded::NotItsLink { .. } => Vec::new(),
        }
    }
}

/// A frame as a runner sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub to: String,
    pub link: String,
    pub open: bool,
    pub data: Vec<u8>,
    pub close: Option<String>,
}

/// How many closed links' ends the relay keeps, for word of what one couldn't read that comes
/// after it closed ([`Relay::ends`]).
pub const CLOSED_KEPT: usize = 4096;

#[derive(Default)]
pub struct Relay {
    links: HashMap<String, Ends>,
    announced: HashMap<String, Announce>,
    /// The ends of the links closed last, newest at the back, at most [`CLOSED_KEPT`].
    closed: std::collections::VecDeque<(String, (String, String))>,
}

impl Relay {
    pub fn announce(&mut self, runner: &str, a: Announce) {
        self.announced.insert(runner.to_owned(), a);
    }

    pub fn announced(&self, runner: &str) -> Option<&Announce> {
        self.announced.get(runner)
    }

    /// A frame from runner `from` on connection `from_peer`. `peer_of` gives a runner's connection
    /// to reach it on (its newest process that takes reach), when it is connected. Returns what
    /// became of it: the frame for the other end, or refused back to the sender, or noted.
    pub fn forward(&mut self, from: &str, from_peer: PeerId, f: Frame, peer_of: impl Fn(&str) -> Option<PeerId>) -> Forwarded {
        let refuse = |code: Refusal, why: String| Forwarded::Refused { to: f.to.clone(), link: f.link.clone(), code, why };
        if f.open {
            if self.links.contains_key(&f.link) {
                return refuse(Refusal::Taken, "that link id is taken".into());
            }
            if f.to == from {
                return refuse(Refusal::ToItself, "a runner does not open a link to itself".into());
            }
            if self.links.values().filter(|e| e.a == from || e.b == from).count() >= LINKS_PER_RUNNER {
                return refuse(Refusal::TooMany, format!("{from} has {LINKS_PER_RUNNER} links open already"));
            }
            match self.announced.get(&f.to) {
                None => return refuse(Refusal::NotOffered, format!("{} does not take reach links (an older runner?)", f.to)),
                Some(a) if !a.accept => return refuse(Refusal::NotAccepted, format!("{} does not accept commands from other runners", f.to)),
                Some(_) => {}
            }
            let Some(to_peer) = peer_of(&f.to) else { return refuse(Refusal::NotConnected, format!("{} is not connected", f.to)) };
            self.links.insert(f.link.clone(), Ends { a: from.to_owned(), a_peer: from_peer, b: f.to.clone(), b_peer: to_peer, since: Instant::now() });
            let closing = f.close.is_some();
            let out = vec![(to_peer, ServerMsg::Reach { from: from.to_owned(), link: f.link.clone(), open: true, data: f.data, close: f.close })];
            if closing {
                self.close(&f.link);
            }
            return Forwarded::Sent(out);
        }
        let Some(ends) = self.links.get(&f.link) else {
            // Gone already (the other end left): say so once, unless this was a goodbye anyway.
            return if f.close.is_some() { Forwarded::Sent(Vec::new()) } else { refuse(Refusal::Closed, "that link is closed".into()) };
        };
        let other = if ends.a == from && ends.a_peer == from_peer {
            ends.b_peer
        } else if ends.b == from && ends.b_peer == from_peer {
            ends.a_peer
        } else {
            // Not an end of this link: nothing is forwarded, and it's noted, naming its ends.
            return Forwarded::NotItsLink { link: f.link, ends: (ends.a.clone(), ends.b.clone()) };
        };
        if f.close.is_some() {
            self.close(&f.link);
        }
        Forwarded::Sent(vec![(other, ServerMsg::Reach { from: from.to_owned(), link: f.link, open: false, data: f.data, close: f.close })])
    }

    /// The two runners at the ends of link `link`, open or closed lately.
    pub fn ends(&self, link: &str) -> Option<(String, String)> {
        match self.links.get(link) {
            Some(e) => Some((e.a.clone(), e.b.clone())),
            None => self.closed.iter().rev().find(|(l, _)| l == link).map(|(_, ends)| ends.clone()),
        }
    }

    /// Link `link` is closed: its ends kept a while ([`Relay::ends`]).
    fn close(&mut self, link: &str) {
        if let Some(e) = self.links.remove(link) {
            if self.closed.len() >= CLOSED_KEPT {
                self.closed.pop_front();
            }
            self.closed.push_back((link.to_owned(), (e.a, e.b)));
        }
    }

    /// A runner's connection went: every link through it is closed, and its other ends told.
    pub fn lost(&mut self, peer: PeerId, why: &str) -> Vec<(PeerId, ServerMsg)> {
        let gone: Vec<(String, Ends)> = self.links.iter().filter(|(_, e)| e.a_peer == peer || e.b_peer == peer).map(|(l, e)| (l.clone(), e.clone())).collect();
        let mut out = Vec::new();
        for (link, e) in gone {
            self.close(&link);
            let (other, left) = if e.a_peer == peer { (e.b_peer, e.a) } else { (e.a_peer, e.b) };
            out.push((other, ServerMsg::Reach { from: left.clone(), link, open: false, data: Vec::new(), close: Some(format!("{left} {why}")) }));
        }
        out
    }

    /// Forget a runner that left for good (removed): its announcement too.
    pub fn forget(&mut self, runner: &str) {
        self.announced.remove(runner);
    }

    pub fn links(&self) -> usize {
        self.links.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(to: &str, link: &str, open: bool, data: &[u8]) -> Frame {
        Frame { to: to.into(), link: link.into(), open, data: data.to_vec(), close: None }
    }

    fn sole(out: Vec<(PeerId, ServerMsg)>) -> anyhow::Result<(PeerId, ServerMsg)> {
        let mut it = out.into_iter();
        let one = it.next().ok_or_else(|| anyhow::anyhow!("nothing sent"))?;
        anyhow::ensure!(it.next().is_none(), "more than one message");
        Ok(one)
    }

    #[test]
    fn frames_go_between_the_two_ends_only() -> anyhow::Result<()> {
        let mut r = Relay::default();
        r.announce("linux", Announce { fingerprint: "f".into(), accept: true, version: 1 });
        let peers = |n: &str| match n {
            "linux" => Some(20),
            "mac" => Some(10),
            _ => None,
        };
        let send = |f: Forwarded| f.sends(10, false);
        // mac (peer 10) opens a link to linux (peer 20).
        let (to, msg) = sole(send(r.forward("mac", 10, frame("linux", "L1", true, b"hello"), peers)))?;
        assert_eq!(to, 20);
        assert_eq!(msg, ServerMsg::Reach { from: "mac".into(), link: "L1".into(), open: true, data: b"hello".to_vec(), close: None });
        // Back from linux, stamped with linux's name, to mac's connection.
        let (to, msg) = sole(r.forward("linux", 20, frame("mac", "L1", false, b"hi"), peers).sends(20, false))?;
        assert_eq!(to, 10);
        assert!(matches!(msg, ServerMsg::Reach { ref from, .. } if from == "linux"));
        // A third runner can't write into it: nothing goes on, and it's noted, naming the ends.
        let ends = ("mac".to_owned(), "linux".to_owned());
        assert_eq!(r.forward("intruder", 30, frame("linux", "L1", false, b"rm -rf"), peers), Forwarded::NotItsLink { link: "L1".into(), ends: ends.clone() });
        // Nor can mac from another connection.
        assert_eq!(r.forward("mac", 11, frame("linux", "L1", false, b"x"), peers), Forwarded::NotItsLink { link: "L1".into(), ends });
        // Opening it again is refused: typed to a runner that takes word of it, in words to one
        // from before.
        let again = r.forward("mac", 10, frame("linux", "L1", true, b""), peers);
        assert!(matches!(&again, Forwarded::Refused { code: Refusal::Taken, .. }));
        let (to, msg) = sole(again.clone().sends(10, true))?;
        assert_eq!(to, 10);
        assert!(matches!(msg, ServerMsg::ReachRefused { code: Refusal::Taken, .. }));
        assert!(matches!(sole(again.sends(10, false))?.1, ServerMsg::Reach { close: Some(_), .. }));
        Ok(())
    }

    #[test]
    fn opening_needs_a_connected_runner_that_accepts() -> anyhow::Result<()> {
        let mut r = Relay::default();
        let none = |_: &str| None;
        let why = |f: Forwarded| match f {
            Forwarded::Refused { code, why, .. } => (code, why),
            _ => (Refusal::Closed, String::new()),
        };
        let (code, w) = why(r.forward("mac", 1, frame("linux", "a", true, b""), none));
        assert!(code == Refusal::NotOffered && w.contains("does not take reach"), "{code:?} {w}");
        r.announce("linux", Announce { fingerprint: "f".into(), accept: false, version: 1 });
        let (code, w) = why(r.forward("mac", 1, frame("linux", "b", true, b""), none));
        assert!(code == Refusal::NotAccepted && w.contains("does not accept"), "{code:?} {w}");
        r.announce("linux", Announce { fingerprint: "f".into(), accept: true, version: 1 });
        let (code, w) = why(r.forward("mac", 1, frame("linux", "c", true, b""), none));
        assert!(code == Refusal::NotConnected && w.contains("not connected"), "{code:?} {w}");
        let (code, w) = why(r.forward("linux", 2, frame("linux", "d", true, b""), |_| Some(2)));
        assert!(code == Refusal::ToItself && w.contains("itself"), "{code:?} {w}");
        let (code, _) = why(r.forward("mac", 1, frame("linux", "gone", false, b"x"), none));
        assert_eq!(code, Refusal::Closed);
        assert_eq!(r.links(), 0);
        Ok(())
    }

    #[test]
    fn a_lost_connection_closes_its_links_and_tells_the_other_end() -> anyhow::Result<()> {
        let mut r = Relay::default();
        r.announce("linux", Announce { fingerprint: "f".into(), accept: true, version: 1 });
        r.announce("mac", Announce { fingerprint: "g".into(), accept: true, version: 1 });
        let peers = |n: &str| (n == "linux").then_some(20).or((n == "mac").then_some(10));
        r.forward("mac", 10, frame("linux", "L1", true, b""), peers);
        r.forward("linux", 20, frame("mac", "L2", true, b""), peers);
        let out = r.lost(20, "disconnected");
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|(p, m)| *p == 10 && matches!(m, ServerMsg::Reach { close: Some(w), .. } if w == "linux disconnected")));
        assert_eq!(r.links(), 0);
        // A goodbye removes the link; frames after it are refused.
        r.forward("mac", 10, frame("linux", "L3", true, b""), peers);
        let mut bye = frame("linux", "L3", false, b"");
        bye.close = Some("done".into());
        assert_eq!(sole(r.forward("mac", 10, bye, peers).sends(10, true))?.0, 20);
        assert_eq!(r.links(), 0);
        Ok(())
    }
}
