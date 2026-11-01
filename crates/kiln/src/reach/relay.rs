//! The server's part in reach (`docs/reach.md` §3.2): it carries a link's frames between the two
//! runners and nothing else. It sees ciphertext and who talks to whom; it never holds a key.
//!
//! A link is opened by its first frame (`open`), which names the runner it is for; the server
//! notes the two ends (each by name and connection) and from then on accepts frames for that link
//! only from those two connections, stamping each with the sender's name as its certificate gives
//! it. When either end's connection goes, the other end is told the link is closed.
//!
//! The relay records what it passes on (docs/ci.md W27-R08's relays): it can't read a frame, so
//! it chains each way's frames by their ciphertext's digests into runs, cut at [`RUN`] frames or
//! [`RUN_FOR`], whichever comes first ([`Relay::due`] cuts one whose frames stopped coming), and the
//! last as the link closes, each for kiln to sign as passed on and send to the runner at the far
//! end, which answers with its receipt of the frames as they came ([`Relay::take_passed`]).

use std::collections::HashMap;
use std::time::Instant;

use hyper_transport::PeerId;
use kiln_ci::identity::relay::Frames;
use serde::{Deserialize, Serialize};

use crate::proto::ServerMsg;

/// Links one runner may have open through the server at once.
pub const LINKS_PER_RUNNER: usize = 256;

/// The most frames of a link's, one way, in a run kiln records as passed on (docs/ci.md W27-R08's
/// relays); its close cuts the last.
pub const RUN: u64 = 1024;

/// How long a run goes on, from its first frame, before the next frame cuts it: a link that's
/// quiet a while (a terminal) has its frames recorded as they flow, not only once it closes.
pub const RUN_FOR: std::time::Duration = std::time::Duration::from_secs(10);

/// A run of a link's frames the relay passed on one way, for kiln to record as passed on
/// ([`Relay::take_passed`]): the link, the runners it went from and to (by name and connection), the
/// run (its frames' ciphertext digests, chained, numbered from the link's first that way), and its
/// number among the link's runs that way, from 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Passed {
    pub link: String,
    pub from: String,
    pub from_peer: PeerId,
    pub to: String,
    pub to_peer: PeerId,
    pub frames: Frames,
    pub seq: u64,
}

/// A link's frames one way since its last run was recorded (from `started`), and the frame
/// before (what a test build's faulty relay swaps in).
#[derive(Debug, Clone)]
struct Run {
    frames: Frames,
    started: Option<Instant>,
    seq: u64,
    before: Option<Vec<u8>>,
}

impl Default for Run {
    fn default() -> Self {
        Self { frames: Frames::new(1), started: None, seq: 0, before: None }
    }
}

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
    /// Its frames each way since their last recorded run: a to b, then b to a.
    runs: [Run; 2],
}

impl Ends {
    /// Its run of frames from `a` (`way` 0) or `b` (1), recorded: none when it holds none.
    fn cut(&mut self, link: &str, way: usize) -> Option<Passed> {
        let r = self.runs.get_mut(way)?;
        if r.frames.is_empty() {
            return None;
        }
        r.seq = r.seq.saturating_add(1);
        r.started = None;
        let next = r.frames.next();
        let frames = std::mem::replace(&mut r.frames, Frames::new(next));
        let ((from, from_peer), (to, to_peer)) = if way == 0 { ((&self.a, self.a_peer), (&self.b, self.b_peer)) } else { ((&self.b, self.b_peer), (&self.a, self.a_peer)) };
        Some(Passed { link: link.to_owned(), from: from.clone(), from_peer, to: to.clone(), to_peer, frames, seq: r.seq })
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

#[derive(Default)]
pub struct Relay {
    links: HashMap<String, Ends>,
    announced: HashMap<String, Announce>,
    /// It records runs of the frames it passes on, for kiln ([`Relay::recording`]).
    recording: bool,
    passed: Vec<Passed>,
}

impl Relay {
    /// A relay that records the runs of frames it passes on, for kiln to sign (docs/ci.md W27-R08's
    /// relays).
    pub fn recording() -> Self {
        Self { recording: true, ..Self::default() }
    }

    /// The runs it recorded since it was last asked: for kiln to sign as passed on, keep, and send to
    /// the runner each went to.
    pub fn take_passed(&mut self) -> Vec<Passed> {
        std::mem::take(&mut self.passed)
    }

    /// Frame `data` of link `link`, passed on from end `way` (0: from `a`): chained into its run
    /// when recording (an empty one, a goodbye, carries nothing to chain), the run recorded once
    /// full. What goes on: the frame, or, from a test build's faulty relay (`relay_frames`), every
    /// second one of each way dropped, swapped for the one before, or with a byte of it changed.
    fn pass(&mut self, link: &str, way: usize, data: Vec<u8>) -> Option<Vec<u8>> {
        use crate::runner::chaos::RelayFault;
        if !self.recording || data.is_empty() {
            return Some(data);
        }
        let Some(ends) = self.links.get_mut(link) else { return Some(data) };
        let Some(run) = ends.runs.get_mut(way) else { return Some(data) };
        let started = *run.started.get_or_insert_with(Instant::now);
        run.frames.push(&data);
        let n = run.frames.next().saturating_sub(1);
        let before = run.before.replace(data.clone());
        let full = run.frames.count() >= RUN || started.elapsed() >= RUN_FOR;
        if full && let Some(p) = ends.cut(link, way) {
            self.passed.push(p);
        }
        match crate::runner::chaos::get().and_then(|c| c.relay_frames).filter(|_| RelayFault::strikes(n)) {
            Some(RelayFault::Drop) => None,
            Some(RelayFault::Swap) => Some(before.unwrap_or(data)),
            Some(RelayFault::Alter) => {
                let mut d = data;
                if let Some(b) = d.last_mut() {
                    *b ^= 1;
                }
                Some(d)
            }
            None => Some(data),
        }
    }

    /// (Tests.) Link `link`'s runs as if they had started `by` earlier.
    #[cfg(test)]
    fn age(&mut self, link: &str, by: std::time::Duration) {
        for r in self.links.get_mut(link).into_iter().flat_map(|e| e.runs.iter_mut()) {
            r.started = r.started.and_then(|t| t.checked_sub(by));
        }
    }

    /// The runs [`RUN_FOR`] old with no frame since to cut them (a link quiet now, or dead with no
    /// goodbye: a frame changed on its way can leave both ends gone without a word), recorded: what
    /// a recording relay passed on is recorded within that long, whether or not more comes.
    pub fn due(&mut self) -> Vec<Passed> {
        if !self.recording {
            return Vec::new();
        }
        for (link, ends) in &mut self.links {
            for way in [0, 1] {
                let old = ends.runs.get(way).and_then(|r| r.started).is_some_and(|t| t.elapsed() >= RUN_FOR);
                if old && let Some(p) = ends.cut(link, way) {
                    self.passed.push(p);
                }
            }
        }
        self.take_passed()
    }

    /// Link `link` ends: what's left of its runs each way, recorded.
    fn end(&mut self, link: &str) {
        let Some(mut ends) = self.links.remove(link) else { return };
        if self.recording {
            self.passed.extend([0, 1].into_iter().filter_map(|way| ends.cut(link, way)));
        }
    }

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
            self.links.insert(f.link.clone(), Ends { a: from.to_owned(), a_peer: from_peer, b: f.to.clone(), b_peer: to_peer, since: Instant::now(), runs: Default::default() });
            let closing = f.close.is_some();
            let data = self.pass(&f.link, 0, f.data);
            let out = data.map(|data| vec![(to_peer, ServerMsg::Reach { from: from.to_owned(), link: f.link.clone(), open: true, data, close: f.close })]).unwrap_or_default();
            if closing {
                self.end(&f.link);
            }
            return out;
        }
        let Some(ends) = self.links.get(&f.link) else {
            // Gone already (the other end left): say so once, unless this was a goodbye anyway.
            return if f.close.is_some() { Vec::new() } else { refuse("that link is closed".into()) };
        };
        let (other, way) = if ends.a == from && ends.a_peer == from_peer {
            (ends.b_peer, 0)
        } else if ends.b == from && ends.b_peer == from_peer {
            (ends.a_peer, 1)
        } else {
            // Not an end of this link: nothing is forwarded, and nothing is said about it.
            return Vec::new();
        };
        let data = self.pass(&f.link, way, f.data);
        if f.close.is_some() {
            self.end(&f.link);
        }
        match data {
            Some(data) => vec![(other, ServerMsg::Reach { from: from.to_owned(), link: f.link, open: false, data, close: f.close })],
            None => Vec::new(),
        }
    }

    /// A runner's connection went: every link through it is closed, and its other ends told.
    pub fn lost(&mut self, peer: PeerId, why: &str) -> Vec<(PeerId, ServerMsg)> {
        let gone: Vec<(String, Ends)> = self.links.iter().filter(|(_, e)| e.a_peer == peer || e.b_peer == peer).map(|(l, e)| (l.clone(), e.clone())).collect();
        let mut out = Vec::new();
        for (link, e) in gone {
            self.end(&link);
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

    /// W27-R08's relays: a recording relay chains each way's frames of a link into runs of [`RUN`],
    /// each recorded with its link, both ends and its number that way, or of what came in
    /// [`RUN_FOR`]; the link's close records what's left each way, and so does a lost connection; a
    /// goodbye's empty frame is nothing to chain; and a relay not recording records nothing.
    #[test]
    fn w27_r08_relays_a_links_frames_are_recorded_in_runs() -> anyhow::Result<()> {
        let mut r = Relay::recording();
        r.announce("linux", Announce { fingerprint: "f".into(), accept: true, version: 1 });
        r.announce("mac", Announce { fingerprint: "g".into(), accept: true, version: 1 });
        let peers = |n: &str| match n {
            "linux" => Some(20),
            "mac" => Some(10),
            _ => None,
        };
        let sent: Vec<Vec<u8>> = (0..RUN + 3).map(|i| i.to_be_bytes().to_vec()).collect();
        r.forward("mac", 10, frame("linux", "L1", true, b"hello"), peers);
        for d in &sent {
            r.forward("mac", 10, frame("linux", "L1", false, d), peers);
        }
        r.forward("linux", 20, frame("mac", "L1", false, b"hi"), peers);
        let passed = r.take_passed();
        let mut want = Frames::new(1);
        want.push(b"hello");
        for d in sent.iter().take(usize::try_from(RUN - 1)?) {
            want.push(d);
        }
        match passed.as_slice() {
            [p] => assert_eq!((p.link.as_str(), p.from.as_str(), p.from_peer, p.to.as_str(), p.to_peer, p.seq, &p.frames), ("L1", "mac", 10, "linux", 20, 1, &want)),
            other => anyhow::bail!("one full run from mac: {other:?}"),
        }
        // Its close: what's left each way; the goodbye itself carries nothing.
        let mut bye = frame("linux", "L1", false, b"");
        bye.close = Some("done".into());
        r.forward("mac", 10, bye, peers);
        let left: Vec<(String, u64, u64, u64)> = r.take_passed().into_iter().map(|p| (p.from, p.seq, p.frames.first(), p.frames.count())).collect();
        assert_eq!(left, vec![("mac".to_owned(), 2, RUN + 1, 4), ("linux".to_owned(), 1, 1, 1)]);
        // A quiet link: its run is cut by the first frame past RUN_FOR, as frames flow.
        r.forward("mac", 10, frame("linux", "L2", true, b"again"), peers);
        r.forward("mac", 10, frame("linux", "L2", false, b"a while"), peers);
        assert!(r.take_passed().is_empty(), "two frames, just now: no run yet");
        r.age("L2", RUN_FOR);
        r.forward("mac", 10, frame("linux", "L2", false, b"later"), peers);
        assert_eq!(r.take_passed().into_iter().map(|p| (p.seq, p.frames.first(), p.frames.count())).collect::<Vec<_>>(), vec![(1, 1, 3)]);
        // A run whose frames stopped coming, cut once RUN_FOR has gone by: not before.
        r.forward("mac", 10, frame("linux", "L2", false, b"then quiet"), peers);
        assert!(r.due().is_empty(), "just now");
        r.age("L2", RUN_FOR);
        assert_eq!(r.due().into_iter().map(|p| (p.seq, p.frames.first(), p.frames.count())).collect::<Vec<_>>(), vec![(2, 4, 1)]);
        assert!(r.due().is_empty(), "cut once");
        // A connection lost: what's left.
        r.forward("mac", 10, frame("linux", "L2", false, b"last"), peers);
        r.lost(20, "disconnected");
        assert_eq!(r.take_passed().into_iter().map(|p| (p.link, p.seq, p.frames.first(), p.frames.count())).collect::<Vec<_>>(), vec![("L2".to_owned(), 3, 5, 1)]);
        // Not recording: nothing.
        let mut quiet = Relay::default();
        quiet.announce("linux", Announce { fingerprint: "f".into(), accept: true, version: 1 });
        quiet.forward("mac", 10, frame("linux", "L3", true, b"hello"), peers);
        assert!(quiet.take_passed().is_empty() && quiet.links() == 1);
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
