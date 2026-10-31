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

#[derive(Clone)]
struct ProcessNonce(String);
impl std::fmt::Debug for ProcessNonce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("ProcessNonce(..)") }
}

/// U03/W13-R09: a checked process and its exact authenticated connection, captured together.
#[derive(Clone, Debug)]
pub struct CapturedRunner {
    machine: String, peer: PeerId, process_nonce: ProcessNonce,
    connection: crate::net::Connection, required: u64, wide: u64, ordinary_epoch: Option<String>,
}
impl CapturedRunner {
    pub fn peer(&self) -> PeerId { self.peer }
    pub(crate) fn machine(&self) -> &str { &self.machine }
    pub fn process_nonce(&self) -> &str { &self.process_nonce.0 }
    pub fn connection_id(&self) -> String { self.connection.connection_id() }
    pub fn wide_caps(&self) -> u64 { self.wide }
    pub(crate) fn ordinary_epoch(&self) -> Option<&str> { self.ordinary_epoch.as_deref() }
    pub(crate) fn connection(&self) -> crate::net::Connection { self.connection }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CaptureRefusal {
    #[error("the selected runner is not connected")]
    Disconnected,
    #[error("the selected runner has not announced the required capabilities on this connection")]
    Capabilities,
    #[error("the selected runner's authenticated connection is not verified or has changed")]
    Connection,
    #[error("the selected runner's process identity is not verified or has changed")]
    Process,
}

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
            | ServerMsg::HarnessSend { session, .. }
            | ServerMsg::HarnessClose { session }
            | ServerMsg::HarnessPackage { session, .. }
            | ServerMsg::HarnessRelease { session }
            | ServerMsg::HarnessFileRead { session, .. }
            | ServerMsg::StreamOpen { session, .. }
            | ServerMsg::StreamControl { session, .. }
            | ServerMsg::StreamInput { session, .. }
            | ServerMsg::StreamClose { session, .. }
            | ServerMsg::Preview { session, .. }
            | ServerMsg::PortOpen { session, .. }
            | ServerMsg::PortData { session, .. }
            | ServerMsg::PortAck { session, .. }
            | ServerMsg::PortClose { session, .. } => Some(Self::Agent(session.clone())),
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
    /// It said which agent sessions it holds since it connected (its inventory): until then what
    /// it holds isn't known here, and it is never retired.
    pub reported: bool,
    /// The build it runs ([`crate::proto::AgentMsg::Build`]); `None` for runners from before.
    pub build: Option<String>,
    /// What it can do ([`crate::proto::CAP_UPGRADE`] ...).
    pub caps: u32,
    /// What it can do past `caps`' 32 bits ([`crate::proto::AgentMsg::Caps`]:
    /// [`crate::proto::CAP_WIRE`] ...); 0 for runners from before.
    pub wide: u64,
    connection: Option<crate::net::Connection>,
    process_nonce: Option<ProcessNonce>,
    ordinary_domain: Option<(crate::net::Connection,String)>,
    /// The kiln it says it runs ([`crate::proto::AgentMsg::Facts`]); `None` for runners from before.
    pub kiln: Option<kiln_ci::facts::KilnFacts>,
}

impl Instance {
    /// Mutable config status requires this instance's declarations from the actual TLS origin;
    /// a replacement socket cannot borrow the former connection's announced capabilities.
    pub(crate) fn on_connection(&self, origin: Option<crate::net::Connection>) -> bool {
        origin.is_some() && self.connection == origin
    }

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
#[cfg_attr(test, derive(ts_rs::TS))]
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

struct ConnectionState { epoch: u64, connection: Option<crate::net::Connection> }

#[derive(Default)]
pub struct Instances {
    by_peer: HashMap<PeerId, Instance>,
    connections: HashMap<PeerId, ConnectionState>,
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
    /// A new TLS connection invalidates a former connection's declarations until Hello/caps.
    pub fn connected(&mut self, peer: PeerId, epoch: u64, binding: Option<[u8; 32]>) {
        self.connections.insert(peer, ConnectionState { epoch, connection: binding.map(|binding|crate::net::Connection::new(peer,epoch,binding)) });
    }
    pub fn accepts_connection(&self, peer: PeerId, origin: Option<crate::net::Connection>) -> bool {
        match self.connections.get(&peer) {
            Some(current) => origin.is_some() && current.connection == origin,
            // Direct in-process legacy fixtures have no TLS event; production always has one.
            None => cfg!(test) && origin.is_none(),
        }
    }
    pub fn accepts_close(&self, peer: PeerId, epoch: u64) -> bool {
        self.connections.get(&peer).is_none_or(|current| current.epoch == epoch)
    }
    pub fn set_process(&mut self, peer: PeerId, nonce: String, origin: Option<crate::net::Connection>) -> Result<(), CaptureRefusal> {
        if !crate::proto::valid_process_nonce(&nonce) { return Err(CaptureRefusal::Process); }
        let connection = origin.ok_or(CaptureRefusal::Connection)?;
        if !self.accepts_connection(peer,Some(connection)) { return Err(CaptureRefusal::Connection); }
        let instance = self.by_peer.get_mut(&peer).ok_or(CaptureRefusal::Disconnected)?;
        if instance.connection != Some(connection) { return Err(CaptureRefusal::Connection); }
        if instance.process_nonce.as_ref().is_some_and(|old| old.0 != nonce) { return Err(CaptureRefusal::Process); }
        instance.process_nonce = Some(ProcessNonce(nonce));
        Ok(())
    }
    /// U01/U03: registration belongs to this actual process/connection, never newest fallback.
    pub(crate) fn set_ordinary_domain(&mut self, peer: PeerId, origin: crate::net::Connection, epoch: String) -> Result<(), CaptureRefusal> {
        if self.process_on(peer,Some(origin)).is_none() { return Err(CaptureRefusal::Process); }
        let instance = self.by_peer.get_mut(&peer).ok_or(CaptureRefusal::Disconnected)?;
        if instance.ordinary_domain.as_ref().is_some_and(|(connection,old)| *connection == origin && old != &epoch) { return Err(CaptureRefusal::Process); }
        instance.ordinary_domain = Some((origin,epoch));
        Ok(())
    }
    /// Shaped startup is eligible only after the process/caps declaration belongs to this live
    /// connection. All older capability meanings remain unchanged.
    pub fn effective_wide(&self, peer: PeerId) -> u64 {
        let raw = self.by_peer.get(&peer).map_or(0,|instance|u64::from(instance.caps)|instance.wide);
        let raw = if self.by_peer.get(&peer).is_some_and(|instance| instance.ordinary_domain.as_ref().is_some_and(|(connection,_)| instance.connection == Some(*connection))) { raw } else { raw & !crate::proto::CAP_ORDINARY_EXECUTION_DEBT };
        let verified = self.connections.get(&peer).and_then(|state|state.connection).is_some_and(|connection|self.process_on(peer,Some(connection)).is_some());
        if !verified || raw & crate::proto::CAP_WIRE == 0 { raw & !crate::proto::CAP_FIRECRACKER_SHAPE } else { raw }
    }
    fn capture_peer(&self, machine: &str, peer: PeerId, expected_nonce: Option<&str>, required: u64) -> Result<CapturedRunner, CaptureRefusal> {
        let instance = self.by_peer.get(&peer).filter(|instance|instance.name==machine).ok_or(CaptureRefusal::Disconnected)?;
        let connection = instance.connection.filter(|connection|self.connections.get(&peer).and_then(|current|current.connection)==Some(*connection)).ok_or(CaptureRefusal::Connection)?;
        let process_nonce = instance.process_nonce.as_ref().ok_or(CaptureRefusal::Process)?;
        if expected_nonce.is_some_and(|expected|expected!=process_nonce.0) { return Err(CaptureRefusal::Process); }
        let wide = self.effective_wide(peer);
        if wide & required != required { return Err(CaptureRefusal::Capabilities); }
        Ok(CapturedRunner { machine: machine.into(), peer, process_nonce: process_nonce.clone(), connection, required, wide, ordinary_epoch: instance.ordinary_domain.as_ref().filter(|(origin,_)| *origin == connection).map(|(_,epoch)| epoch.clone()) })
    }
    pub fn capture_newest_runner(&self, machine: &str, required: u64) -> Result<CapturedRunner, CaptureRefusal> {
        self.capture_peer(machine,self.newest(machine).ok_or(CaptureRefusal::Disconnected)?,None,required)
    }
    pub fn capture_original_runner(&self, machine: &str, peer: PeerId, process_nonce: &str, required: u64) -> Result<CapturedRunner, CaptureRefusal> {
        self.capture_peer(machine,peer,Some(process_nonce),required)
    }
    pub fn process_on(&self, peer: PeerId, origin: Option<crate::net::Connection>) -> Option<&str> {
        let connection = origin?;
        self.by_peer.get(&peer).filter(|instance|instance.connection==Some(connection)).and_then(|instance|instance.process_nonce.as_ref()).map(|nonce|nonce.0.as_str())
    }
    #[cfg(test)]
    pub fn proves_process(&self, peer: PeerId, origin: Option<crate::net::Connection>, nonce: &str) -> bool {
        self.process_on(peer,origin)==Some(nonce)
    }
    pub fn still_captured(&self, captured: &CapturedRunner) -> bool {
        self.capture_peer(&captured.machine,captured.peer,Some(&captured.process_nonce.0),captured.required).is_ok_and(|current|current.connection==captured.connection)
    }

    /// [`crate::proto::AgentMsg::Instance`], which precedes Hello.
    pub fn announce(&mut self, peer: PeerId, ih: InstanceHello) {
        self.announced.insert(peer, ih);
    }

    pub fn join(&mut self, peer: PeerId, name: &str, hello: Hello, now: i64) -> Joined {
        let ih = self.announced.remove(&peer);
        let id = ih.as_ref().map(|i| i.id.clone());
        let connection = self.connections.get(&peer).and_then(|current|current.connection);
        let process_nonce = self.by_peer.get(&peer).filter(|old| old.connection == connection).and_then(|old|old.process_nonce.clone());
        // The same instance (same id, or the one legacy runner) on another peer is a stale
        // connection it left behind.
        let stale: Vec<PeerId> =
            self.by_peer.iter().filter(|(p, i)| **p != peer && i.name == name && i.id == id).map(|(p, _)| *p).collect();
        for p in &stale {
            self.by_peer.remove(p);
            self.connections.remove(p);
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
                reported: false,
                build: None,
                caps: 0,
                wide: 0,
                connection,
                process_nonce,
                ordinary_domain: None,
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

    /// Which of `name`'s processes holds what one process of a machine holds at a time (its replica
    /// of kiln's records: `caps` [`crate::proto::CAP_CLUSTER`]): its newest, if it can, and the
    /// others that can, which let theirs go. A blue/green upgrade's new build joins ranked oldest,
    /// so it lets go until it's promoted, and holds it from then on.
    pub fn holder_of(&self, name: &str, caps: u32) -> (Option<PeerId>, Vec<PeerId>) {
        let newest = self.newest_with(name, caps);
        let mut others: Vec<PeerId> = self.by_peer.iter().filter(|(p, i)| i.name == name && Some(**p) != newest && i.caps & caps == caps).map(|(p, _)| *p).collect();
        others.sort_unstable();
        (newest, others)
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

    /// What it can do past `Build`'s 32 bits ([`crate::proto::AgentMsg::Caps`]).
    pub fn set_wide(&mut self, peer: PeerId, caps: u64) {
        if let Some(i) = self.by_peer.get_mut(&peer) {
            i.wide = caps;
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
        self.connections.remove(&peer);
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
    /// What the runner process of machine `name` that a message about `what` would go to can do
    /// (its caps): the one that holds it, else the newest; 0 when none is connected.
    pub fn caps_for(&self, name: &str, what: &Owned) -> u32 {
        let peer = self.owner_on(name, what).or_else(|| self.newest(name));
        peer.and_then(|p| self.get(p)).map_or(0, |i| i.caps)
    }

    /// [`Instances::caps_for`] in 64 bits: `Build`'s 32 and what it said past them
    /// ([`crate::proto::AgentMsg::Caps`]).
    pub fn wide_caps_for(&self, name: &str, what: &Owned) -> u64 {
        let peer = self.owner_on(name, what).or_else(|| self.newest(name));
        peer.and_then(|p| self.get(p)).map_or(0, |i| u64::from(i.caps) | i.wide)
    }

    /// The instance of machine `name` that holds `what`: none when none does (one of another
    /// machine that held it, before it went on there, doesn't count).
    fn owner_on(&self, name: &str, what: &Owned) -> Option<PeerId> {
        self.owner(what).filter(|p| self.by_peer.get(p).is_some_and(|i| i.name == name))
    }

    pub fn route(&mut self, name: &str, msg: &ServerMsg) -> Option<PeerId> {
        let what = Owned::of(msg);
        let held = what.as_ref().filter(|_| !Owned::creates(msg)).and_then(|w| self.owner_on(name, w));
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

    /// Runner process `peer` said which agent sessions it holds (its inventory).
    pub fn reported(&mut self, peer: PeerId) {
        if let Some(i) = self.by_peer.get_mut(&peer) {
            i.reported = true;
        }
    }

    /// Of two runner processes of one machine, the newer (the one new work goes to). None when
    /// either is gone, or they are of different machines.
    pub fn newer_of(&self, a: PeerId, b: PeerId) -> Option<PeerId> {
        let (x, y) = (self.by_peer.get(&a)?, self.by_peer.get(&b)?);
        (x.name == y.name).then(|| if x.rank() >= y.rank() { a } else { b })
    }

    /// Whether runner process `peer` takes hand-overs between its machine's processes
    /// ([`crate::proto::CAP_HANDOVER`]): never a blue/green upgrade's new build before it takes
    /// over (it ranks oldest meanwhile, and the runner it replaces moves its sessions to it itself:
    /// `crate::bluegreen`).
    pub fn hands_over(&self, peer: PeerId) -> bool {
        self.by_peer.get(&peer).is_some_and(|i| i.wide & crate::proto::CAP_HANDOVER != 0 && !(i.id.is_some() && i.created_at < 0))
    }

    /// Whether machine `name` is being upgraded blue/green: a new build of its runner is connected
    /// and hasn't taken over yet. Its newest process meanwhile is the one handing everything to it,
    /// and leaving.
    pub fn upgrading(&self, name: &str) -> bool {
        self.by_peer.values().any(|i| i.name == name && i.id.is_some() && i.created_at < 0)
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

    /// A machine's replica is its newest process's (docs/p2p.md, P2): a blue/green upgrade's new
    /// build lets it go until it takes over, then holds it, and the runner it took over from lets
    /// go. So the server says who holds it again as a new build is promoted (hub.rs
    /// `newest_changed`), not only as a process says what it can do.
    #[test]
    fn a_new_build_holds_its_machines_replica_once_it_has_taken_over() {
        use crate::proto::CAP_CLUSTER;
        let mut inst = Instances::default();
        inst.announce(1, ih("main", 0));
        inst.join(1, "m", hello("1"), 0);
        inst.set_build(1, "old".into(), CAP_CLUSTER);
        assert_eq!(inst.holder_of("m", CAP_CLUSTER), (Some(1), vec![]));
        inst.announce(2, ih("green", -1));
        inst.join(2, "m", hello("2"), 0);
        inst.set_build(2, "new".into(), CAP_CLUSTER);
        assert_eq!(inst.holder_of("m", CAP_CLUSTER), (Some(1), vec![2]), "before it takes over, the new build lets the replica go");
        assert_eq!(inst.reannounce(2, &ih("green", 100)), Some(vec![1]));
        assert_eq!(inst.holder_of("m", CAP_CLUSTER), (Some(2), vec![1]), "promoted, it holds it, and the old runner lets go");
        // A newest process that can't hold one: none holds it, and the one that could lets go.
        inst.set_build(2, "new".into(), 0);
        assert_eq!(inst.holder_of("m", CAP_CLUSTER), (None, vec![1]));
    }

    #[test]
    fn hand_overs_are_between_processes_that_take_them_never_a_new_build_before_it_takes_over() {
        use crate::proto::{CAP_HANDOVER, CAP_WIRE};
        let mut m = Instances::default();
        m.announce(1, ih("main", 0));
        m.join(1, "box", hello("a"), 1);
        m.announce(2, ih("0199-new", 100));
        m.join(2, "box", hello("b"), 2);
        m.join(3, "other", hello("c"), 3);
        assert_eq!(m.newer_of(1, 2), Some(2));
        assert_eq!(m.newer_of(2, 1), Some(2));
        assert_eq!(m.newer_of(1, 3), None, "of different machines");
        assert_eq!(m.newer_of(1, 9), None, "one gone");
        assert!(!m.hands_over(1), "a runner from before says nothing of it");
        m.set_wide(1, CAP_WIRE | CAP_HANDOVER);
        m.set_wide(2, CAP_HANDOVER);
        assert!(m.hands_over(1) && m.hands_over(2));
        // A blue/green upgrade's new build, before it takes over: ranked oldest, and left alone.
        m.announce(4, ih("green", -1));
        m.join(4, "box", hello("d"), 4);
        m.set_wide(4, CAP_HANDOVER);
        assert!(!m.hands_over(4));
        assert!(m.upgrading("box") && !m.upgrading("other"), "its machine is being upgraded meanwhile");
        assert_eq!(m.reannounce(4, &ih("green", 200)), Some(vec![2]), "the one drained before stays so");
        assert!(m.hands_over(4), "once it took over, it's a process like the others");
        assert!(!m.upgrading("box"));
    }

    #[test]
    fn a_process_has_said_what_it_holds_only_once_its_inventory_came() {
        let mut m = Instances::default();
        m.join(1, "box", hello("a"), 1);
        assert!(m.get(1).is_some_and(|i| !i.reported), "just connected: not yet");
        m.reported(1);
        assert!(m.get(1).is_some_and(|i| i.reported));
        // The same process on a new connection says it again.
        m.join(2, "box", hello("a"), 2);
        assert!(m.get(2).is_some_and(|i| !i.reported));
        m.reported(9);
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

    /// CP-T3.2: a session owned by an instance of another machine (its copy left behind there,
    /// which answered a message) is sent to on the machine it runs on: its newest instance.
    #[test]
    fn an_owner_on_another_machine_does_not_take_a_sessions_messages() {
        let mut m = Instances::default();
        m.join(1, "old-box", hello("a"), 1);
        m.join(2, "new-box", hello("b"), 2);
        m.own(Owned::Agent("s1".into()), 1);
        let send = ServerMsg::HarnessSend { session: "s1".into(), epoch: 1, json: "{}".into() };
        assert_eq!(m.route("new-box", &send), Some(2));
        assert_eq!(m.caps_for("new-box", &Owned::Agent("s1".into())), m.get(2).map_or(0, |i| i.caps));
        // On its own machine, its owner keeps it.
        assert_eq!(m.route("old-box", &send), Some(1));
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

    /// U03/W13-R09: selection, process nonce and TLS connection remain one authority.
    #[test]
    fn w13_r09_captured_startup_never_falls_back_on_reconnect_or_downgrade() -> anyhow::Result<()> {
        let (peer, newer) = (1,2);
        let (nonce, other_nonce) = ("a".repeat(43),"b".repeat(43));
        let (a,b,c) = (crate::net::Connection::new(peer,1,[1;32]),crate::net::Connection::new(peer,2,[2;32]),crate::net::Connection::new(newer,1,[3;32]));
        let caps = crate::proto::CAP_FIRECRACKER_SHAPE | crate::proto::CAP_WIRE;
        let mut m = Instances::default();
        m.connected(peer,1,Some([1;32]));
        m.announce(peer,ih("original",1));
        m.join(peer,"box",hello("capable"),1);
        m.set_wide(peer,caps);
        assert_eq!(m.effective_wide(peer)&crate::proto::CAP_FIRECRACKER_SHAPE,0,"caps without process association are not startup eligibility");
        m.set_process(peer,nonce.clone(),Some(a))?;
        assert_ne!(m.effective_wide(peer)&crate::proto::CAP_FIRECRACKER_SHAPE,0);
        let captured = m.capture_newest_runner("box",caps)?;
        assert!(m.still_captured(&captured));
        m.connected(peer,2,Some([2;32]));
        assert_eq!(m.effective_wide(peer)&crate::proto::CAP_FIRECRACKER_SHAPE,0,"replacement invalidates existing scheduler eligibility");
        assert!(!m.accepts_connection(peer,Some(a)));
        assert!(!m.still_captured(&captured));
        assert!(!m.accepts_close(peer,1), "an old close cannot clear a replacement connection");
        assert!(m.accepts_close(peer,2));
        assert!(m.capture_newest_runner("box",caps).is_err());
        assert!(m.set_process(peer,other_nonce.clone(),Some(a)).is_err());
        m.announce(peer,ih("original",1));
        m.join(peer,"box",hello("reconnected"),2);
        m.set_process(peer,nonce.clone(),Some(b))?;
        assert!(matches!(m.capture_original_runner("box",peer,&nonce,caps),Err(CaptureRefusal::Capabilities)));
        m.set_wide(peer,caps);
        let resumed = m.capture_original_runner("box",peer,&nonce,caps)?;
        m.set_wide(peer,crate::proto::CAP_WIRE);
        assert_eq!(m.effective_wide(peer)&crate::proto::CAP_FIRECRACKER_SHAPE,0,"downgraded caps immediately lose startup eligibility");
        m.set_wide(peer,caps);
        assert_ne!(captured.connection_id(),resumed.connection_id());
        assert!(m.proves_process(peer,Some(b),&nonce));
        assert!(!m.proves_process(peer,Some(a),&nonce));
        assert!(m.set_process(peer,other_nonce.clone(),Some(b)).is_err());
        m.join(peer,"box",hello("repeated hello"),3);
        assert!(m.set_process(peer,other_nonce.clone(),Some(b)).is_err(),"Hello cannot reset the same connection's process identity");
        m.set_wide(peer,caps);
        m.connected(newer,1,Some([3;32]));
        m.announce(newer,ih("newest",2));
        m.join(newer,"box",hello("old binary"),4);
        m.set_process(newer,other_nonce.clone(),Some(c))?;
        assert!(matches!(m.capture_newest_runner("box",caps),Err(CaptureRefusal::Capabilities)),"newest capability loss must not select an older capable process");
        assert!(m.capture_original_runner("box",peer,&nonce,caps).is_ok());
        assert!(m.capture_original_runner("box",newer,&nonce,caps).is_err());
        Ok(())
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
