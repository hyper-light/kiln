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

/// A frame as a runner sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub to: String,
    pub link: String,
    pub open: bool,
    pub data: Vec<u8>,
    pub close: Option<String>,
}

#[derive(Default)]
pub struct Relay {
    links: HashMap<String, Ends>,
    announced: HashMap<String, Announce>,
}

impl Relay {
    pub fn announce(&mut self, runner: &str, a: Announce) {
        self.announced.insert(runner.to_owned(), a);
    }

    pub fn announced(&self, runner: &str) -> Option<&Announce> {
        self.announced.get(runner)
    }

    /// A frame from runner `from` on connection `from_peer`. `peer_of` gives a runner's connection
    /// to reach it on (its newest process that takes reach), when it is connected. Returns what to
    /// send where: the frame to the other end, or a close back to the sender.
    pub fn forward(&mut self, from: &str, from_peer: PeerId, f: Frame, peer_of: impl Fn(&str) -> Option<PeerId>) -> Vec<(PeerId, ServerMsg)> {
        let refuse = |why: String| vec![(from_peer, ServerMsg::Reach { from: f.to.clone(), link: f.link.clone(), open: false, data: Vec::new(), close: Some(why) })];
        if f.open {
            if self.links.contains_key(&f.link) {
                return refuse("that link id is taken".into());
            }
            if f.to == from {
                return refuse("a runner does not open a link to itself".into());
            }
            if self.links.values().filter(|e| e.a == from || e.b == from).count() >= LINKS_PER_RUNNER {
                return refuse(format!("{from} has {LINKS_PER_RUNNER} links open already"));
            }
            match self.announced.get(&f.to) {
                None => return refuse(format!("{} does not take reach links (an older runner?)", f.to)),
                Some(a) if !a.accept => return refuse(format!("{} does not accept commands from other runners", f.to)),
                Some(_) => {}
            }
            let Some(to_peer) = peer_of(&f.to) else { return refuse(format!("{} is not connected", f.to)) };
            self.links.insert(f.link.clone(), Ends { a: from.to_owned(), a_peer: from_peer, b: f.to.clone(), b_peer: to_peer, since: Instant::now() });
            let closing = f.close.is_some();
            let out = vec![(to_peer, ServerMsg::Reach { from: from.to_owned(), link: f.link.clone(), open: true, data: f.data, close: f.close })];
            if closing {
                self.links.remove(&f.link);
            }
            return out;
        }
        let Some(ends) = self.links.get(&f.link) else {
            // Gone already (the other end left): say so once, unless this was a goodbye anyway.
            return if f.close.is_some() { Vec::new() } else { refuse("that link is closed".into()) };
        };
        let other = if ends.a == from && ends.a_peer == from_peer {
            ends.b_peer
        } else if ends.b == from && ends.b_peer == from_peer {
            ends.a_peer
        } else {
            // Not an end of this link: nothing is forwarded, and nothing is said about it.
            return Vec::new();
        };
        if f.close.is_some() {
            self.links.remove(&f.link);
        }
        vec![(other, ServerMsg::Reach { from: from.to_owned(), link: f.link, open: false, data: f.data, close: f.close })]
    }

    /// A runner's connection went: every link through it is closed, and its other ends told.
    pub fn lost(&mut self, peer: PeerId, why: &str) -> Vec<(PeerId, ServerMsg)> {
        let gone: Vec<(String, Ends)> = self.links.iter().filter(|(_, e)| e.a_peer == peer || e.b_peer == peer).map(|(l, e)| (l.clone(), e.clone())).collect();
        let mut out = Vec::new();
        for (link, e) in gone {
            self.links.remove(&link);
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
        // mac (peer 10) opens a link to linux (peer 20).
        let (to, msg) = sole(r.forward("mac", 10, frame("linux", "L1", true, b"hello"), peers))?;
        assert_eq!(to, 20);
        assert_eq!(msg, ServerMsg::Reach { from: "mac".into(), link: "L1".into(), open: true, data: b"hello".to_vec(), close: None });
        // Back from linux, stamped with linux's name, to mac's connection.
        let (to, msg) = sole(r.forward("linux", 20, frame("mac", "L1", false, b"hi"), peers))?;
        assert_eq!(to, 10);
        assert!(matches!(msg, ServerMsg::Reach { ref from, .. } if from == "linux"));
        // A third runner can't write into it, and learns nothing.
        assert!(r.forward("intruder", 30, frame("linux", "L1", false, b"rm -rf"), peers).is_empty());
        // Nor can mac from another connection.
        assert!(r.forward("mac", 11, frame("linux", "L1", false, b"x"), peers).is_empty());
        // Opening it again is refused.
        let (to, msg) = sole(r.forward("mac", 10, frame("linux", "L1", true, b""), peers))?;
        assert_eq!(to, 10);
        assert!(matches!(msg, ServerMsg::Reach { close: Some(_), .. }));
        Ok(())
    }

    #[test]
    fn opening_needs_a_connected_runner_that_accepts() -> anyhow::Result<()> {
        let mut r = Relay::default();
        let none = |_: &str| None;
        let why = |out: Vec<(PeerId, ServerMsg)>| match out.into_iter().next() {
            Some((_, ServerMsg::Reach { close: Some(w), .. })) => w,
            _ => String::new(),
        };
        assert!(why(r.forward("mac", 1, frame("linux", "a", true, b""), none)).contains("does not take reach"));
        r.announce("linux", Announce { fingerprint: "f".into(), accept: false, version: 1 });
        assert!(why(r.forward("mac", 1, frame("linux", "b", true, b""), none)).contains("does not accept"));
        r.announce("linux", Announce { fingerprint: "f".into(), accept: true, version: 1 });
        assert!(why(r.forward("mac", 1, frame("linux", "c", true, b""), none)).contains("not connected"));
        assert!(why(r.forward("linux", 2, frame("linux", "d", true, b""), |_| Some(2))).contains("itself"));
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
        assert_eq!(sole(r.forward("mac", 10, bye, peers))?.0, 20);
        assert_eq!(r.links(), 0);
        Ok(())
    }
}
