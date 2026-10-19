//! A machine's runner processes ("instances"). A machine normally connects with one runner. To
//! upgrade one whose runner cannot take a new binary in place (older runners), a new runner is
//! started beside it with its own certificate for the same machine name: both stay connected,
//! new work (terminals, agent sessions, jobs) goes to the newest, and the old one keeps everything
//! it already runs until that ends — then it is retired. Nothing is restarted, nothing is lost.
//!
//! This is the bookkeeping: which instances each machine has, which one is newest, and which
//! instance owns each terminal, agent session and job, so messages about them reach the process
//! that holds them. Pure data (no I/O); the hub drives it.

use std::collections::HashMap;
use std::time::Instant;

use serde::Serialize;

use crate::proto::{Hello, InstanceHello, ServerMsg};

pub type PeerId = u64;

/// What an instance owns, by kind and id.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Owned {
    Terminal(String),
    Agent(String),
    Job(String),
}

impl Owned {
    /// The thing a server message is about, if it is about one thing an instance holds.
    pub fn of(msg: &ServerMsg) -> Option<Self> {
        match msg {
            ServerMsg::TermOpen { session, .. }
            | ServerMsg::TermInput { session, .. }
            | ServerMsg::TermResize { session, .. }
            | ServerMsg::TermClose { session }
            | ServerMsg::TermAttach { session } => Some(Self::Terminal(session.clone())),
            ServerMsg::HarnessOpen { session, .. }
            | ServerMsg::HarnessCmd { session, .. }
            | ServerMsg::HarnessClose { session }
            | ServerMsg::HarnessPackage { session, .. }
            | ServerMsg::HarnessRelease { session }
            | ServerMsg::HarnessFileRead { session, .. }
            | ServerMsg::StreamOpen { session, .. }
            | ServerMsg::StreamControl { session, .. }
            | ServerMsg::StreamInput { session, .. }
            | ServerMsg::StreamClose { session, .. } => Some(Self::Agent(session.clone())),
            ServerMsg::Assign(a) => Some(Self::Job(a.job_id.clone())),
            // The CI engine's assignment carries its leg's id inside its JSON (docs/ci.md §2.5).
            ServerMsg::AssignV2 { json } => assign_v2_job(json).map(Self::Job),
            ServerMsg::Cancel { job, .. } => Some(Self::Job(job.clone())),
            _ => None,
        }
    }

    /// Messages that create the thing: whoever they are sent to owns it from then on.
    fn creates(msg: &ServerMsg) -> bool {
        matches!(msg, ServerMsg::TermOpen { .. } | ServerMsg::HarnessOpen { .. } | ServerMsg::HarnessPackage { .. } | ServerMsg::Assign(_) | ServerMsg::AssignV2 { .. })
    }
}

/// The leg an `AssignV2` is for.
fn assign_v2_job(json: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Head {
        job_id: String,
    }
    serde_json::from_str::<Head>(json).ok().map(|h| h.job_id)
}

#[derive(Debug, Clone)]
pub struct Instance {
    pub name: String,
    /// `None`: a runner from before instances, which never says which one it is ("legacy").
    pub id: Option<String>,
    pub created_at: i64,
    pub pid: Option<u32>,
    pub hello: Hello,
    pub connected_at: i64,
    /// When the newest instance was last told this one can be retired.
    pub retire_sent: Option<Instant>,
    /// It was told to stop taking jobs (it is not the newest).
    pub drained: bool,
    /// The build it runs ([`crate::proto::AgentMsg::Build`]); `None` for runners from before.
    pub build: Option<String>,
    /// What it can do ([`crate::proto::CAP_UPGRADE`] ...).
    pub caps: u32,
    /// The kiln it says it runs ([`crate::proto::AgentMsg::Facts`]); `None` for runners from before.
    pub kiln: Option<kiln_ci::facts::KilnFacts>,
}

impl Instance {
    /// Newer instances sort later; a legacy runner is the oldest of all.
    fn rank(&self) -> (i64, &str) {
        match &self.id {
            None => (i64::MIN, ""),
            Some(id) => (self.created_at, id.as_str()),
        }
    }

    pub fn label(&self) -> String {
        self.id.clone().unwrap_or_else(|| "legacy".to_owned())
    }
}

/// An instance as the UI shows it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct InstanceView {
    pub id: String,
    pub legacy: bool,
    pub version: String,
    pub created_at: i64,
    pub connected_at: i64,
    pub pid: Option<u32>,
    /// New work goes here.
    pub newest: bool,
    /// What still keeps an older instance running.
    pub terminals: usize,
    pub agents: usize,
    pub jobs: usize,
    /// The build it runs, when it says (runners from before builds were reported don't).
    pub build: Option<String>,
    /// It takes an upgrade command; older runners are upgraded through a terminal instead.
    pub can_upgrade: bool,
}

#[derive(Default)]
pub struct Instances {
    by_peer: HashMap<PeerId, Instance>,
    announced: HashMap<PeerId, InstanceHello>,
    owners: HashMap<Owned, PeerId>,
}

/// What happened when an instance said hello.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Joined {
    /// Connections of this same instance that are stale (it reconnected with a new peer id):
    /// drop them.
    pub stale: Vec<PeerId>,
    /// Older instances of the machine that should stop taking jobs (Drain).
    pub drain: Vec<PeerId>,
    /// This instance is now the machine's newest.
    pub newest: bool,
}

impl Instances {
    /// [`crate::proto::AgentMsg::Instance`], which precedes Hello.
    pub fn announce(&mut self, peer: PeerId, ih: InstanceHello) {
        self.announced.insert(peer, ih);
    }

    pub fn join(&mut self, peer: PeerId, name: &str, hello: Hello, now: i64) -> Joined {
        let ih = self.announced.remove(&peer);
        let id = ih.as_ref().map(|i| i.id.clone());
        // The same instance (same id, or the one legacy runner) on another peer is a stale
        // connection it left behind.
        let stale: Vec<PeerId> =
            self.by_peer.iter().filter(|(p, i)| **p != peer && i.name == name && i.id == id).map(|(p, _)| *p).collect();
        for p in &stale {
            self.by_peer.remove(p);
        }
        self.by_peer.insert(
            peer,
            Instance {
                name: name.to_owned(),
                id,
                created_at: ih.as_ref().map_or(0, |i| i.created_at),
                pid: ih.map(|i| i.pid),
                hello,
                connected_at: now,
                retire_sent: None,
                drained: false,
                build: None,
                caps: 0,
                kiln: None,
            },
        );
        // Ownership moves with the instance.
        for owner in self.owners.values_mut() {
            if stale.contains(owner) {
                *owner = peer;
            }
        }
        let newest_peer = self.newest_of(name);
        let newest = newest_peer == Some(peer);
        let mut drain = Vec::new();
        for (p, i) in &mut self.by_peer {
            if i.name == name && Some(*p) != newest_peer && !i.drained {
                i.drained = true;
                drain.push(*p);
            }
        }
        drain.sort_unstable();
        Joined { stale, drain, newest }
    }

    /// An instance that has joined says again when it was set up: a blue/green upgrade's new build
    /// (`crate::bluegreen`) joins ranked oldest, so new work stays with the runner it takes over
    /// from, and once promoted says when it really was set up, which makes it the newest. `None`
    /// when `peer` hasn't joined (its hello is still to come); else the machine's instances that
    /// should stop taking jobs now (Drain).
    pub fn reannounce(&mut self, peer: PeerId, ih: &InstanceHello) -> Option<Vec<PeerId>> {
        let i = self.by_peer.get_mut(&peer)?;
        i.created_at = ih.created_at;
        i.pid = Some(ih.pid);
        let name = i.name.clone();
        let newest = self.newest_of(&name);
        let mut drain = Vec::new();
        for (p, i) in &mut self.by_peer {
            if i.name == name && Some(*p) != newest && !i.drained {
                i.drained = true;
                drain.push(*p);
            }
        }
        drain.sort_unstable();
        Some(drain)
    }

    fn newest_of(&self, name: &str) -> Option<PeerId> {
        self.by_peer.iter().filter(|(_, i)| i.name == name).max_by(|a, b| a.1.rank().cmp(&b.1.rank())).map(|(p, _)| *p)
    }

    /// The machine's newest connected instance: where new work goes.
    pub fn newest(&self, name: &str) -> Option<PeerId> {
        self.newest_of(name)
    }

    /// The machine's newest instance if it announced every capability in `caps`
    /// ([`crate::proto::CAP_DIRS`] ...), so it is never sent a message it can't read. An older
    /// instance that could is not asked instead: new work goes to the newest.
    pub fn newest_with(&self, name: &str, caps: u32) -> Option<PeerId> {
        self.newest_of(name).filter(|p| self.by_peer.get(p).is_some_and(|i| i.caps & caps == caps))
    }

    /// [`crate::proto::AgentMsg::Facts`]: the kiln an instance says it runs.
    pub fn set_kiln(&mut self, peer: PeerId, kiln: kiln_ci::facts::KilnFacts) {
        if let Some(i) = self.by_peer.get_mut(&peer) {
            i.kiln = Some(kiln);
        }
    }

    /// [`crate::proto::AgentMsg::Build`]: which build an instance runs and what it can do.
    pub fn set_build(&mut self, peer: PeerId, build: String, caps: u32) {
        if let Some(i) = self.by_peer.get_mut(&peer) {
            i.build = Some(build);
            i.caps = caps;
        }
    }

    pub fn get(&self, peer: PeerId) -> Option<&Instance> {
        self.by_peer.get(&peer)
    }

    pub fn get_mut(&mut self, peer: PeerId) -> Option<&mut Instance> {
        self.by_peer.get_mut(&peer)
    }

    pub fn of(&self, name: &str) -> Vec<PeerId> {
        let mut v: Vec<PeerId> = self.by_peer.iter().filter(|(_, i)| i.name == name).map(|(p, _)| *p).collect();
        v.sort_unstable();
        v
    }

    /// The connection went away. Returns the instance and what it owned.
    pub fn lost(&mut self, peer: PeerId) -> Option<(Instance, Vec<Owned>)> {
        self.announced.remove(&peer);
        let i = self.by_peer.remove(&peer)?;
        let owned = self.owned_by(peer);
        Some((i, owned))
    }

    pub fn owned_by(&self, peer: PeerId) -> Vec<Owned> {
        self.owners.iter().filter(|(_, p)| **p == peer).map(|(o, _)| o.clone()).collect()
    }

    pub fn own(&mut self, what: Owned, peer: PeerId) {
        self.owners.insert(what, peer);
    }

    pub fn disown(&mut self, what: &Owned) {
        self.owners.remove(what);
    }

    pub fn owner(&self, what: &Owned) -> Option<PeerId> {
        self.owners.get(what).copied()
    }

    /// Where a message for machine `name` goes: to the instance holding what it is about, else
    /// (new things, and anything not about one thing) to the newest. A message that creates a
    /// thing makes that instance its owner.
    pub fn route(&mut self, name: &str, msg: &ServerMsg) -> Option<PeerId> {
        let what = Owned::of(msg);
        let held = what.as_ref().filter(|_| !Owned::creates(msg)).and_then(|w| self.owner(w)).filter(|p| self.by_peer.contains_key(p));
        let peer = held.or_else(|| self.newest(name))?;
        if let Some(w) = what
            && Owned::creates(msg)
        {
            self.owners.insert(w, peer);
        }
        Some(peer)
    }

    pub fn views(&self, name: &str, count: impl Fn(PeerId) -> (usize, usize, usize)) -> Vec<InstanceView> {
        let newest = self.newest(name);
        let mut v: Vec<InstanceView> = self
            .by_peer
            .iter()
            .filter(|(_, i)| i.name == name)
            .map(|(p, i)| {
                let (terminals, agents, jobs) = count(*p);
                InstanceView {
                    id: i.label(),
                    legacy: i.id.is_none(),
                    version: i.hello.version.clone(),
                    created_at: i.created_at,
                    connected_at: i.connected_at,
                    pid: i.pid,
                    newest: Some(*p) == newest,
                    terminals,
                    agents,
                    jobs,
                    build: i.build.clone(),
                    can_upgrade: i.caps & crate::proto::CAP_UPGRADE != 0,
                }
            })
            .collect();
        v.sort_by(|a, b| b.newest.cmp(&a.newest).then(a.connected_at.cmp(&b.connected_at)));
        v
    }

    /// Every connected peer of every machine with more than one instance, with its machine.
    pub fn crowded(&self) -> Vec<(String, PeerId)> {
        let mut by_name: HashMap<&str, Vec<PeerId>> = HashMap::new();
        for (p, i) in &self.by_peer {
            by_name.entry(i.name.as_str()).or_default().push(*p);
        }
        let mut out: Vec<(String, PeerId)> =
            by_name.into_iter().filter(|(_, ps)| ps.len() > 1).flat_map(|(n, ps)| ps.into_iter().map(move |p| (n.to_owned(), p))).collect();
        out.sort();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(version: &str) -> Hello {
        Hello {
            protocol: crate::proto::VERSION,
            name: "box".into(),
            labels: vec![],
            slots: 1,
            hostname: "box".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            version: version.into(),
            ephemeral: false,
            dedicated_job: None,
            running: vec![],
            terminal: true,
        }
    }

    fn ih(id: &str, created_at: i64) -> InstanceHello {
        InstanceHello { id: id.into(), created_at, pid: 1 }
    }

    #[test]
    fn a_new_runner_beside_a_legacy_one_takes_new_work_and_the_old_keeps_its_own() -> anyhow::Result<()> {
        let mut m = Instances::default();
        // The old runner (never announces itself) owns a terminal.
        let j = m.join(10, "box", hello("old"), 1);
        assert_eq!(j, Joined { stale: vec![], drain: vec![], newest: true });
        assert_eq!(m.route("box", &ServerMsg::TermOpen { session: "t1".into(), cols: 80, rows: 24, command: None }), Some(10));

        // A new runner joins beside it: it becomes the newest; the old one is drained, not dropped.
        m.announce(20, ih("0199-new", 100));
        let j = m.join(20, "box", hello("new"), 2);
        assert_eq!(j, Joined { stale: vec![], drain: vec![10], newest: true });
        assert_eq!(m.of("box"), vec![10, 20]);

        // The old terminal still reaches the old process; a new one goes to the new process.
        assert_eq!(m.route("box", &ServerMsg::TermInput { session: "t1".into(), data: vec![] }), Some(10));
        assert_eq!(m.route("box", &ServerMsg::TermOpen { session: "t2".into(), cols: 80, rows: 24, command: None }), Some(20));
        assert_eq!(m.route("box", &ServerMsg::TermResize { session: "t2".into(), cols: 1, rows: 1 }), Some(20));
        // Messages about nothing in particular go to the newest.
        assert_eq!(m.route("box", &ServerMsg::AiTranscript { req: 1, pid: 1 }), Some(20));

        // The legacy runner reconnects (blip): same instance, so it replaces its stale peer and
        // keeps its terminal; it stays drained and is not the newest.
        let j = m.join(11, "box", hello("old"), 3);
        assert_eq!(j.stale, vec![10]);
        assert!(!j.newest);
        assert_eq!(j.drain, vec![11]);
        assert_eq!(m.route("box", &ServerMsg::TermInput { session: "t1".into(), data: vec![] }), Some(11));

        // The old process goes away: what it owned is reported.
        let (gone, owned) = m.lost(11).ok_or_else(|| anyhow::anyhow!("unknown peer"))?;
        assert!(gone.id.is_none());
        assert_eq!(owned, vec![Owned::Terminal("t1".into())]);
        assert_eq!(m.newest("box"), Some(20));
        assert!(m.crowded().is_empty());
        Ok(())
    }

    #[test]
    fn a_new_build_takes_new_work_once_it_says_it_has_taken_over() {
        let mut inst = Instances::default();
        inst.announce(1, ih("main", 0));
        inst.join(1, "m", hello("1"), 0);
        // The new build joins ranked oldest: new work stays with the runner it takes over from.
        inst.announce(2, ih("green", -1));
        let joined = inst.join(2, "m", hello("2"), 0);
        assert!(!joined.newest);
        assert_eq!(joined.drain, vec![2]);
        assert_eq!(inst.newest("m"), Some(1));
        // Promoted, it says when it was set up: the newest now, and the old runner is drained.
        assert_eq!(inst.reannounce(2, &ih("green", 100)), Some(vec![1]));
        assert_eq!(inst.newest("m"), Some(2));
        assert_eq!(inst.reannounce(2, &ih("green", 100)), Some(vec![]), "drained once");
        // Before its hello, an announcement is the one its hello comes with.
        assert_eq!(inst.reannounce(3, &ih("other", 5)), None);
    }

    #[test]
    fn the_newest_is_by_creation_not_by_who_connected_last() {
        let mut m = Instances::default();
        m.announce(1, ih("a", 100));
        m.join(1, "box", hello("a"), 1);
        m.announce(2, ih("b", 200));
        m.join(2, "box", hello("b"), 2);
        // `a` restarts and reconnects later; `b` stays the newest.
        m.lost(1);
        m.announce(3, ih("a", 100));
        let j = m.join(3, "box", hello("a"), 3);
        assert!(!j.newest);
        assert_eq!(m.newest("box"), Some(2));
        assert_eq!(m.crowded(), vec![("box".to_owned(), 2), ("box".to_owned(), 3)]);
    }

    #[test]
    fn unknown_or_disconnected_owners_fall_back_to_the_newest() {
        let mut m = Instances::default();
        m.join(1, "box", hello("x"), 1);
        assert_eq!(m.route("box", &ServerMsg::TermInput { session: "nobody-knows".into(), data: vec![] }), Some(1));
        m.own(Owned::Terminal("t".into()), 99);
        assert_eq!(m.route("box", &ServerMsg::TermInput { session: "t".into(), data: vec![] }), Some(1));
        assert_eq!(m.route("other", &ServerMsg::TermInput { session: "t".into(), data: vec![] }), None);
    }

    #[test]
    fn folder_listings_only_go_to_a_newest_runner_that_announced_them() {
        use crate::proto::{CAP_DIRS, CAP_TAKEOVER, CAP_UPGRADE};
        let mut m = Instances::default();
        assert_eq!(m.newest_with("box", CAP_DIRS), None);
        // A legacy runner announces nothing; one from before CAP_DIRS announces the rest.
        m.join(1, "box", hello("legacy"), 1);
        assert_eq!(m.newest_with("box", CAP_DIRS), None);
        m.set_build(1, "b1".into(), CAP_UPGRADE | CAP_TAKEOVER);
        assert_eq!(m.newest_with("box", CAP_DIRS), None);
        assert_eq!(m.newest_with("box", CAP_TAKEOVER), Some(1));

        // A current runner joins beside it: nothing until its Build says CAP_DIRS.
        m.announce(2, ih("new", 100));
        m.join(2, "box", hello("new"), 2);
        assert_eq!(m.newest_with("box", CAP_DIRS), None);
        m.set_build(2, "b2".into(), CAP_UPGRADE | CAP_TAKEOVER | CAP_DIRS);
        assert_eq!(m.newest_with("box", CAP_DIRS), Some(2));

        // A newer runner without it (a downgrade): the older one that could is not asked instead.
        m.announce(3, ih("newer", 200));
        m.join(3, "box", hello("newer"), 3);
        m.set_build(3, "b3".into(), CAP_UPGRADE);
        assert_eq!(m.newest_with("box", CAP_DIRS), None);
        m.lost(3);
        assert_eq!(m.newest_with("box", CAP_DIRS), Some(2));
        assert_eq!(m.newest_with("other", CAP_DIRS), None);
    }
}
