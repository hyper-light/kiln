//! One part's group as one node takes part in it: the node's member of the group, serving the part
//! on an endpoint of its own (Focal's `focal_bus::member`, doc 34), and the node's publisher to the
//! group, a Focal client on an endpoint of its own that sends each publish to the group's leader.
//!
//! **The publisher.** A publish is a Focal batch of one record in the publisher's session (its
//! producer, a sequence for each batch), sent to the node it believes leads. A node that doesn't
//! lead answers a typed redirect naming the leader it knows (doc 34 P2), and the publisher follows
//! it where its term is past the one it knew (KIP-951's rule). Once sent, a batch is sent again,
//! in order, until the group has committed it: whenever the publisher's leader changes, its
//! connection comes up, the part answers that the session waits for an earlier batch, or a
//! period passes without an answer. The part's sessions recognise a batch it took before and
//! never append it twice (doc 34 P6). A batch never sent, for no leader was known within the
//! node's quorum wait, is refused [`quiver_api::Refusal::NoQuorum`]: nothing of it is kept.
//!
//! **What's committed.** Every member applies what its group committed to its part, the leader
//! and followers alike; [`Cursor`] feeds the node what its member's part took, in order, from the
//! oldest it keeps. A publish is answered once its node's own member has applied it, at the
//! offset the part gave it: the publish is on a majority's disks (the persisted class's
//! acknowledgement, the full flush), and a read on the node that published it finds it.

use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::task::Waker;
use std::time::{Duration, Instant};

use focal_bus::batch::{Batch as FocalBatch, BatchBuilder, Producing};
use focal_bus::budget::BusBudget;
use focal_bus::client::{Client, Delivery};
use focal_bus::directory::BusDirectory;
use focal_bus::log::Position;
use focal_bus::member::{GroupEvent, Member, MemberSettings};
use focal_bus::node::{Node as FocalNode, NodeConfig as FocalNodeConfig};
use focal_bus::part::Part;
use focal_bus::pki::{self, Identity, Local};
use focal_bus::wire::{Ack, Bus, Durability, Role};
use focal_memory::MemoryBudget;
use hyper_rt_transport::Driver;
use hyper_transport::tls::{CertificateDer, Credentials, PrivateKeyDer, PrivatePkcs8KeyDer, Tls};
use hyper_transport::{Config, Endpoint, Event, Refusal as WireRefusal, RestartKey};

/// A node's endpoint, driven on its host's shard.
pub(crate) type Drv = Driver<Bus, BusBudget, BusDirectory>;

/// Format: the stream a part's batches are on, one topic a part.
pub(crate) const STREAM: u64 = 1;

/// The bytes each endpoint's transport may hold, charged as its buffers and windows grow: Focal's
/// own group test's figure (focal-bus `tests/group.rs`), which holds three nodes and a client.
const TRANSPORT_MEMORY: usize = 64 << 20;

/// Each part's bounds beside its memory: one stream, no subscriptions over the wire (the node
/// reads its own part), and the sessions of every node's publisher with room to spare.
const PART_STREAMS: usize = 4;
const PART_SUBSCRIPTIONS: usize = 4;
const PART_PRODUCERS: usize = 64;

/// How long a publisher waits for an answer before it sends again what it sent: a resend of a
/// batch in flight costs the leader a lookup (it owes the first copy's acknowledgement already,
/// `Member::publish`), so this bounds only how long a lost connection's batches wait, beside the
/// transport's own loss recovery.
const RESEND: Duration = Duration::from_millis(250);
/// How long a publisher hears nothing from the node it sends to, batches owed, before it tries the
/// next: a leader killed -9 says nothing, and its connection ends only at the transport's idle
/// timeout (two minutes, `focal_bus::limits::IDLE_TIMEOUT`), so the publisher asks another node,
/// which leads or names the leader it knows. Twice [`RESEND`]: a leader that answers within a
/// resend is never left.
const SILENT: Duration = Duration::from_millis(500);
/// How long a publisher waits before trying another node, after one knew no leader.
const RETRY: Duration = Duration::from_millis(50);
/// A dial to a peer that went: the first after this, each after twice the one before, up to
/// [`REDIAL_MOST`]. A peer back from a restart is found within a second; one gone for good costs
/// a handshake a second.
const REDIAL_FIRST: Duration = Duration::from_millis(50);
const REDIAL_MOST: Duration = Duration::from_secs(1);
/// The most batches a feed takes in one turn: the rest wait for the next, which is due at once.
const FEED_MOST: usize = 4096;

/// The cluster's identities (`focal_bus::pki`, the same directory at every node): each node's,
/// and each node's publisher's, a client identity of its own.
pub(crate) struct Identities {
    local: Local,
    nodes: usize,
}

fn credentials(local: &Local, own: &Identity) -> Credentials {
    Credentials {
        chain: vec![CertificateDer::from(own.certificate.clone())],
        key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(own.key.clone())),
        roots: vec![CertificateDer::from(local.root.clone())],
    }
}

/// A Focal configuration error, said.
fn said(error: impl std::fmt::Display) -> String {
    error.to_string()
}

/// An endpoint made now on the shard's clock (`hyper_rt_transport::shard_instant`, the host's off
/// a simulated shard), its QUIC layer seeded with `seed` where one is given, keyed by `restart`, the
/// key it keeps across its restarts at its address, where one is given.
fn made(
    config: Config<Role>,
    budget: BusBudget,
    directory: BusDirectory,
    seed: Option<[u8; 32]>,
    restart: Option<&RestartKey>,
) -> Result<Endpoint<Bus, BusBudget, BusDirectory>, String> {
    let now = hyper_rt_transport::shard_instant();
    match (restart, seed) {
        (Some(key), seed) => Endpoint::new_restartable(config, budget, directory, now, key, seed),
        (None, Some(seed)) => Endpoint::new_seeded(config, budget, directory, now, seed),
        (None, None) => Endpoint::new(config, budget, directory, now),
    }
    .map_err(|e| format!("{e:?}"))
}

impl Identities {
    /// The identities of a cluster of `nodes`, kept in `dir` (made there the first time, with
    /// `kind`'s keys), sealed under `key_file`, or under the per-user root key file named for the
    /// directory's root key where it names none, as Focal's node keeps its own (focal-bus `serve`).
    pub(crate) fn load(
        dir: &std::path::Path,
        key_file: Option<&std::path::Path>,
        nodes: usize,
        kind: pki::KeyKind,
    ) -> Result<Self, String> {
        let local = pki::load_or_make_with(dir, key_file, nodes, nodes, kind).map_err(said)?;
        Ok(Self { local, nodes })
    }

    fn client_index(id: u64) -> Option<usize> {
        usize::try_from(id).ok()?.checked_sub(1)
    }

    fn node_ids(&self) -> impl Iterator<Item = u64> {
        (1..=u64::try_from(self.nodes).unwrap_or(0)).take(self.nodes)
    }

    /// Node `id`'s endpoint for a member: it knows the other nodes and every node's publisher. Its
    /// QUIC layer is seeded with `seed` where one is given (the simulation's), from the system
    /// otherwise, and it is keyed by the key it keeps across its restarts at `bound`, the address its
    /// socket holds.
    pub(crate) fn member_endpoint(
        &self,
        id: u64,
        seed: Option<[u8; 32]>,
        bound: SocketAddr,
    ) -> Result<Endpoint<Bus, BusBudget, BusDirectory>, String> {
        let peers = self.nodes.saturating_sub(1);
        let mut directory = BusDirectory::new(self.nodes.saturating_add(peers));
        for node in self.node_ids().filter(|n| *n != id) {
            let identity = self.local.node(node).ok_or("no identity for a node")?;
            directory
                .enroll(
                    &identity.certificate,
                    node,
                    Role::Node,
                    &pki::node_name(node),
                )
                .map_err(said)?;
        }
        for (index, client) in self.local.clients.iter().enumerate() {
            let peer = self.local.client_peer(index).ok_or("a client's peer")?;
            let name = format!("focal-bus-client-{}", index.saturating_add(1));
            directory
                .enroll(&client.certificate, peer, Role::Client, &name)
                .map_err(said)?;
        }
        let own = self.local.node(id).ok_or("no identity for this node")?;
        let limits = focal_bus::limits::node(self.nodes, peers).map_err(said)?;
        let config = Config {
            tls: Tls::mutual(&credentials(&self.local, own), true).map_err(|e| format!("{e:?}"))?,
            role: Role::Node,
            limits,
            congestion: Default::default(),
        };
        let budget = BusBudget::new(MemoryBudget::new(TRANSPORT_MEMORY, 0).map_err(said)?);
        // Keyed as Focal's node keys its endpoint (`serve::restart_key`): a peer that sends on a
        // connection the node's last run held at `bound` is answered with a reset and dials again,
        // where it held the dead run's connection until its idle timeout and refused the new run's
        // dials as Busy (hyper-raft docs/transport.md §4ah; Quiver's simulation, seed 3763).
        let restart = focal_bus::serve::restart_key(&own.key, bound).map_err(said)?;
        made(config, budget, directory, seed, Some(&restart))
    }

    /// Node `id`'s endpoint for a publisher: it knows every node. Seeded as a member's is; never
    /// keyed for restarts, as its port is the system's choice at each start, where no peer reaches
    /// its last run.
    pub(crate) fn client_endpoint(
        &self,
        id: u64,
        seed: Option<[u8; 32]>,
    ) -> Result<Endpoint<Bus, BusBudget, BusDirectory>, String> {
        let mut directory = BusDirectory::new(self.nodes);
        for node in self.node_ids() {
            let identity = self.local.node(node).ok_or("no identity for a node")?;
            directory
                .enroll(
                    &identity.certificate,
                    node,
                    Role::Node,
                    &pki::node_name(node),
                )
                .map_err(said)?;
        }
        let own = Self::client_index(id)
            .and_then(|index| self.local.clients.get(index))
            .ok_or("no client identity for this node")?;
        let config = Config {
            tls: Tls::mutual(&credentials(&self.local, own), false)
                .map_err(|e| format!("{e:?}"))?,
            role: Role::Client,
            limits: focal_bus::limits::client_of(self.nodes).map_err(said)?,
            congestion: Default::default(),
        };
        let budget = BusBudget::new(MemoryBudget::new(TRANSPORT_MEMORY, 0).map_err(said)?);
        made(config, budget, directory, seed, None)
    }

    /// Each node, and the peer its publisher is to every member (`pki::Local::client_peer`).
    pub(crate) fn publishers(&self) -> Vec<(u64, u64)> {
        self.node_ids()
            .filter_map(|node| {
                let index = usize::try_from(node.checked_sub(1)?).ok()?;
                Some((node, self.local.client_peer(index)?))
            })
            .collect()
    }

    /// The lane window a member's acknowledgements are bounded by (`Member::open`).
    pub(crate) fn lane_window(&self) -> Result<usize, String> {
        Ok(
            focal_bus::limits::node(self.nodes, self.nodes.saturating_sub(1))
                .map_err(said)?
                .lane_window,
        )
    }

    pub(crate) const fn nodes(&self) -> usize {
        self.nodes
    }
}

/// Whether `event` is a connection to a peer coming up (true) or going (false).
fn peer_change(event: &Event<Bus>) -> Option<(u64, bool)> {
    match event {
        Event::Connected { peer, .. } => Some((*peer, true)),
        Event::Closed { peer, .. } | Event::Unreachable { peer, .. } => Some((*peer, false)),
        _ => None,
    }
}

/// The peers an endpoint dials again once their connections go.
#[derive(Debug, Default)]
pub(crate) struct Redial {
    /// Each peer whose connection went: when to dial it next, and the wait after that.
    due: BTreeMap<u64, (Instant, Duration)>,
}

impl Redial {
    fn note(&mut self, peer: u64, up: bool, now: Instant) {
        if up {
            self.due.remove(&peer);
        } else {
            let wait = self.due.get(&peer).map_or(REDIAL_FIRST, |(_, wait)| *wait);
            let next = wait.saturating_mul(2).min(REDIAL_MOST);
            self.due
                .insert(peer, (now.checked_add(wait).unwrap_or(now), next));
        }
    }

    fn deadline(&self) -> Option<Instant> {
        self.due.values().map(|(at, _)| *at).min()
    }

    /// Under the simulation, the peers due to be dialed at `now`, with when each fell due.
    #[cfg(feature = "sim")]
    fn due_now(&self, now: Instant) -> Vec<(u64, Instant)> {
        self.due
            .iter()
            .filter(|(_, (at, _))| *at <= now)
            .map(|(peer, (at, _))| (*peer, *at))
            .collect()
    }

    /// Dials each peer of `peers` that is due; a dial that fails is due again, later.
    fn dial(
        &mut self,
        endpoint: &mut Endpoint<Bus, BusBudget, BusDirectory>,
        peers: &[(u64, SocketAddr)],
        now: Instant,
    ) {
        let due: Vec<u64> = self
            .due
            .iter()
            .filter(|(_, (at, _))| *at <= now)
            .map(|(peer, _)| *peer)
            .collect();
        for peer in due {
            if let Some((_, address)) = peers.iter().find(|(id, _)| *id == peer) {
                let _ = endpoint.connect(now, peer, *address);
            }
            // Due again unless it comes up; its wait was doubled as it went.
            if let Some((at, wait)) = self.due.get_mut(&peer) {
                *at = now.checked_add(*wait).unwrap_or(now);
                *wait = wait.saturating_mul(2).min(REDIAL_MOST);
            }
        }
    }
}

/// What a member is opened from, kept to open it again after it fences.
#[derive(Debug, Clone)]
pub(crate) struct Plan {
    pub(crate) id: u64,
    pub(crate) peers: Vec<(u64, SocketAddr)>,
    pub(crate) data: PathBuf,
    pub(crate) key_file: PathBuf,
    pub(crate) memory: u64,
    pub(crate) members: Vec<u64>,
    pub(crate) clients: usize,
    pub(crate) lane_window: usize,
    /// Each node, and the peer its publisher is to this member (`Identities::publishers`).
    pub(crate) publishers: Vec<(u64, u64)>,
    /// Under the simulation, the node's simulated machine its log's device is on.
    #[cfg(feature = "sim")]
    pub(crate) machine: Option<std::sync::Arc<super::sim::Machine>>,
}

impl Plan {
    fn open(&self, now: Instant) -> Result<Member, String> {
        let memory = usize::try_from(self.memory).map_err(said)?;
        let budget = focal_bus::limits::group_budget(memory).map_err(said)?;
        let node = FocalNode::new(
            FocalNodeConfig {
                segment_bytes: FocalNodeConfig::SEGMENT_BYTES,
                streams: PART_STREAMS,
                subscriptions: PART_SUBSCRIPTIONS,
                producers: PART_PRODUCERS,
            },
            budget.clone(),
        )
        .map_err(said)?;
        let settings = MemberSettings {
            id: self.id,
            peers: self.peers.clone(),
            data: self.data.clone(),
            key_file: self.key_file.clone(),
            memory_bytes: self.memory,
            // No fates: the group commits on a majority's disks, the persisted class.
            fates: Vec::new(),
            spanning: 0,
        };
        let (members, clients, window) = (self.members.clone(), self.clients, self.lane_window);
        #[cfg(feature = "sim")]
        if let Some(machine) = &self.machine {
            let simulated = focal_bus::member::Simulated {
                device: machine.handle(&self.data)?,
                settings,
            };
            #[cfg_attr(not(feature = "plant-own-log-commit"), allow(unused_mut))]
            let mut member =
                Member::open_simulated(simulated, node, members, clients, window, budget, now)
                    .map_err(said)?;
            // The planted bug's build: a commit on the leader's own log alone.
            #[cfg(feature = "plant-own-log-commit")]
            member.plant(Some(hyper_raft::Mutant::OwnLogCommit));
            return persisted(member);
        }
        let member =
            Member::open(settings, node, members, clients, window, budget, now).map_err(said)?;
        persisted(member)
    }
}

/// `member`, if it acknowledges on a majority's disks: the persisted class.
fn persisted(member: Member) -> Result<Member, String> {
    if member.class() != Durability::Persisted {
        return Err("the part's group would not acknowledge on a majority's disks".to_owned());
    }
    Ok(member)
}

/// This node's member of a part's group, on its endpoint.
pub(crate) struct Seat {
    pub(crate) driver: Drv,
    pub(crate) member: Member,
    plan: Plan,
    redial: Redial,
    /// Under the simulation, the member's last connection events and dials, with when (the shard's
    /// clock, ms), for its diagnosis.
    #[cfg(feature = "sim")]
    heard: VecDeque<String>,
}

impl Seat {
    /// The member `plan` names, on `driver`, dialing every other node: members open as each node
    /// learns its peers' ports, and a node back from a restart is dialed by no one (its peers'
    /// connections to it ended with its process, unknown to them until they suspect it).
    pub(crate) fn open(mut driver: Drv, plan: Plan, now: Instant) -> Result<Self, String> {
        let member = plan.open(now)?;
        for (peer, address) in &plan.peers {
            let _ = driver.endpoint().connect(now, *peer, *address);
        }
        #[cfg_attr(not(feature = "sim"), allow(unused_mut))]
        let mut seat = Self {
            driver,
            member,
            plan,
            redial: Redial::default(),
            #[cfg(feature = "sim")]
            heard: VecDeque::new(),
        };
        #[cfg(feature = "sim")]
        seat.hear(format!("opens, dials {:?}", seat.plan.peers));
        Ok(seat)
    }

    /// Under the simulation, a connection event or a dial noted for the diagnosis.
    #[cfg(feature = "sim")]
    fn hear(&mut self, what: String) {
        if self.heard.len() >= 64 {
            self.heard.pop_front();
        }
        let ms = hyper_rt::futures::now_ns() / 1_000_000;
        self.heard.push_back(format!("{ms} ms: {what}"));
    }

    pub(crate) fn on_event(&mut self, event: Event<Bus>, now: Instant) {
        #[cfg(feature = "sim")]
        if let Some((peer, up)) = peer_change(&event) {
            self.hear(format!("peer {peer} {}", if up { "up" } else { "down" }));
        }
        if let Some((peer, up)) = peer_change(&event)
            && self.plan.peers.iter().any(|(id, _)| *id == peer)
        {
            self.redial.note(peer, up, now);
        }
        self.member.on_event(self.driver.endpoint(), event, now);
    }

    /// The member's turn, its peers dialed where due; a member that fenced is opened again from
    /// what its log made durable (Focal's rule: a fenced replica takes nothing until reopened).
    ///
    /// A peer the member's liveness stream suspects, having heard from it, has its connections
    /// closed and is dialed again. A connection is replaced only once it carried nothing for the transport's serve
    /// period (two minutes, hyper-transport's admission), and this node goes on sending on the
    /// connections of a peer that was killed, both of an identity's places (two a peer,
    /// `focal_bus::limits::CONNECTIONS_PER_CLIENT`) taken where each side dialed the other:
    /// without this, a peer back from a kill was refused every connection it dialed
    /// (`Refusal::Busy`) and never caught up (CP-T2.2's kill drills, 2026-10-25).
    pub(crate) fn turn(&mut self, waker: &Waker, now: Instant) -> Result<(), String> {
        #[cfg(feature = "sim")]
        for (peer, at) in self.redial.due_now(now) {
            self.hear(format!("dials {peer}, due since {at:?}"));
        }
        self.redial
            .dial(self.driver.endpoint(), &self.plan.peers, now);
        let mut suspected = Vec::new();
        let mut restarted = Vec::new();
        self.member.turn(
            self.driver.endpoint(),
            waker,
            now,
            &mut |event| match event {
                // A peer it heard from before: one never heard from (its member not yet open) holds
                // no connection that ended with its process, and closing what it holds only delays it.
                GroupEvent::Suspected(suspicion) if suspicion.last.is_some() => {
                    suspected.push(suspicion.peer);
                }
                GroupEvent::Restarted { peer } => restarted.push(*peer),
                _ => {}
            },
        );
        // A node back from a kill within its peers' detection bound is never suspected: they hear
        // only that it restarted, its stream's later run. Its dead run's publisher connections held
        // both of its client identity's places, and every member refused the new run's publisher
        // Busy until they had carried nothing for two minutes (the sweep at d4ad260, seeds 1293
        // and 2608: 3,718 connections lost, no acknowledgement). Its member's own connections need
        // nothing: the new run answers the dead run's with a reset (its restart key).
        for peer in restarted {
            let publisher = self
                .plan
                .publishers
                .iter()
                .find(|(node, _)| *node == peer)
                .map(|(_, client)| *client);
            if self.plan.peers.iter().any(|(id, _)| *id == peer)
                && let Some(client) = publisher
            {
                #[cfg(feature = "sim")]
                self.hear(format!("{peer} restarted: disconnects its publisher"));
                self.driver.endpoint().disconnect(now, client);
            }
        }
        for peer in suspected {
            if self.plan.peers.iter().any(|(id, _)| *id == peer) {
                #[cfg(feature = "sim")]
                self.hear(format!("suspects {peer}: disconnects it and its publisher"));
                self.driver.endpoint().disconnect(now, peer);
                self.redial.note(peer, false, now);
                // Its node's publisher too: a node back from a kill dials with the same client
                // identity, and the dead process's connections held both of its places, so every
                // member refused its publisher Busy until they had carried nothing for two minutes
                // (CP-T2.2 step 6's simulation, seed 8). The publisher dials again, as it does.
                let publisher = self.plan.publishers.iter().find(|(node, _)| *node == peer);
                if let Some((_, client)) = publisher {
                    self.driver.endpoint().disconnect(now, *client);
                }
            }
        }
        if self.member.fenced() {
            self.member = self.plan.open(now)?;
        }
        Ok(())
    }

    /// Takes `everyone`'s addresses for its peers; whether one changed. A peer that moved (its
    /// member bound another port as it started again, the one it kept taken) has what this node
    /// holds or is dialing at its old address closed, and is dialed at its new one: a dial in
    /// flight to the old address held off every new one until its handshake timed out, two
    /// minutes, the pair apart all that time (CP-T2.2 step 6's simulation, seed 3).
    pub(crate) fn peers_changed(&mut self, everyone: &[(u64, SocketAddr)], now: Instant) -> bool {
        let peers: Vec<(u64, SocketAddr)> = everyone
            .iter()
            .filter(|(id, _)| *id != self.plan.id)
            .copied()
            .collect();
        if peers == self.plan.peers {
            return false;
        }
        for (peer, address) in &peers {
            let moved = self
                .plan
                .peers
                .iter()
                .any(|(id, before)| id == peer && before != address);
            if moved {
                #[cfg(feature = "sim")]
                self.hear(format!(
                    "peer {peer} moved to {address}: closes and dials it there"
                ));
                self.driver.endpoint().disconnect(now, *peer);
                self.redial.note(*peer, false, now);
            }
        }
        self.plan.peers = peers;
        true
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        match (self.member.deadline(), self.redial.deadline()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    pub(crate) fn part(&self) -> &Part {
        self.member.node().part()
    }

    /// Under the simulation, everything the member and its endpoint say of themselves.
    #[cfg(feature = "sim")]
    pub(crate) fn diagnosis(&mut self) -> String {
        let leader = self.member.leader();
        let mut established = Vec::new();
        self.driver.endpoint().established(&mut established);
        let paths: Vec<String> = established
            .iter()
            .map(|(peer, at)| {
                let quic = self.driver.endpoint().connection_stats(*peer);
                format!("{peer}@{at}: {quic:?}")
            })
            .collect();
        format!(
            "member of node {} at {}: leads {}, leader {:?} in term {}, in flight {:?}, fenced {}\n  core {}\n  {:?}\n  endpoint {:?}\n  dials {:?}\n  established {paths:?}\n  heard {:?}\n  redial {:?}",
            self.plan.id,
            self.plan.data.display(),
            self.member.is_leader(),
            leader.node,
            leader.term,
            self.member.in_flight(),
            self.member.fenced(),
            self.member.core_state(),
            self.member.stats(),
            self.driver.endpoint().stats(),
            self.plan.peers,
            self.heard,
            self.redial,
        )
    }

    /// Under the simulation, what a wait of `what` that never answered finds of the member: its
    /// work in flight and its log's frames, flushes and the flush on its device, if one is.
    #[cfg(feature = "sim")]
    pub(crate) fn stuck(&self, what: &str) -> String {
        let (writes, reads) = self.member.in_flight();
        let stats = self.member.stats();
        format!(
            "the simulation's {what} of node {}'s member at {} never answered: {writes} writes and \
             {reads} reads in flight, its log's frames {}, flushes {}",
            self.plan.id,
            self.plan.data.display(),
            stats.log.frames,
            stats.log.flushes,
        )
    }
}

/// An endpoint bound for a member not yet opened (its peers' ports not all known): whatever comes
/// is let go, its peers sending again once the member is up.
pub(crate) fn drop_event(driver: &mut Drv, event: Event<Bus>) {
    if let Event::Frame { frame, .. } = event {
        driver.endpoint().release(frame);
    }
}

/// A publish asked and not yet sent.
#[derive(Debug)]
struct Queued {
    record: Vec<u8>,
    at_us: u64,
    asked: Instant,
    ticket: u64,
}

/// A batch sent and not yet seen committed.
#[derive(Debug)]
struct Owed {
    /// The batch, while it may need sending again: none once a leader acknowledged it.
    batch: Option<Vec<u8>>,
    ticket: u64,
    /// The offset its acknowledgement gave it.
    base: Option<u64>,
}

/// What a publisher's answers settled.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Settled {
    /// The part refused the batch alike at every node: its ticket, refused.
    Refused(u64),
}

/// What a publisher did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PublisherStats {
    pub(crate) sent: u64,
    pub(crate) resent: u64,
    pub(crate) redirects: u64,
    pub(crate) acks: u64,
    pub(crate) connects: u64,
    pub(crate) losses: u64,
    /// Dials the transport refused as they were asked.
    pub(crate) dials_refused: u64,
}

/// This node's publisher to a part's group (module docs).
pub(crate) struct Publisher {
    pub(crate) driver: Drv,
    /// Where each of the group's members listens, as known so far; none until every one is.
    nodes: Vec<(u64, SocketAddr)>,
    connected: Vec<u64>,
    redial: Redial,
    /// The node it sends to, whether it knows that node leads, and the term it knows.
    target: Option<u64>,
    leads: bool,
    term: u64,
    /// The node it prefers while it knows no leader: its own.
    prefer: u64,
    /// The session its batches are in, and the sequence the next takes.
    pub(crate) producer: u64,
    next: u32,
    builder: BatchBuilder,
    owed: BTreeMap<u32, Owed>,
    queued: VecDeque<Queued>,
    resend_at: Option<Instant>,
    retry_at: Option<Instant>,
    /// When the node it sends to, not connected, is dialed again.
    dial_at: Option<Instant>,
    /// When it last knew a leader, or was made.
    led_at: Instant,
    /// When the node it sends to last answered, or it began sending there.
    answered_at: Instant,
    pub(crate) stats: PublisherStats,
    /// Under the simulation, what it chose and what its connections did, the latest 64, and its
    /// sends by node and the redirects it took by sender and leader, for the diagnosis.
    #[cfg(feature = "sim")]
    pub(crate) heard: VecDeque<String>,
    #[cfg(feature = "sim")]
    pub(crate) counts: BTreeMap<String, u64>,
}

impl Publisher {
    pub(crate) fn new(driver: Drv, producer: u64, prefer: u64, now: Instant) -> Self {
        Self {
            driver,
            nodes: Vec::new(),
            connected: Vec::new(),
            redial: Redial::default(),
            target: None,
            leads: false,
            term: 0,
            prefer,
            producer,
            next: 0,
            builder: BatchBuilder::new(),
            owed: BTreeMap::new(),
            queued: VecDeque::new(),
            resend_at: None,
            retry_at: None,
            dial_at: None,
            #[cfg(feature = "sim")]
            heard: VecDeque::new(),
            #[cfg(feature = "sim")]
            counts: BTreeMap::new(),
            led_at: now,
            answered_at: now,
            stats: PublisherStats::default(),
        }
    }

    /// The group's members, every one's address known: the publisher may send.
    pub(crate) fn set_nodes(&mut self, nodes: Vec<(u64, SocketAddr)>, now: Instant) {
        self.nodes = nodes;
        self.led_at = now;
        self.choose(None, now);
    }

    /// Takes `nodes`' addresses for the group's members, going on as it was. A member that moved
    /// has what the publisher holds or is dialing at its old address closed: a dial in flight
    /// there held off a new one for two minutes, as the member's own did (`Seat::peers_changed`).
    pub(crate) fn set_addresses(&mut self, nodes: &[(u64, SocketAddr)], now: Instant) {
        for (node, address) in nodes {
            let moved = self
                .nodes
                .iter()
                .any(|(id, before)| id == node && before != address);
            if moved {
                self.driver.endpoint().disconnect(now, *node);
            }
        }
        nodes.clone_into(&mut self.nodes);
    }

    pub(crate) fn has_nodes(&self) -> bool {
        !self.nodes.is_empty()
    }

    pub(crate) fn queue(&mut self, record: Vec<u8>, at_us: u64, ticket: u64, now: Instant) {
        self.queued.push_back(Queued {
            record,
            at_us,
            asked: now,
            ticket,
        });
    }

    /// Under the simulation, a choice or a connection's change noted for the diagnosis.
    #[cfg(feature = "sim")]
    fn hear(&mut self, what: String) {
        if self.heard.len() >= 64 {
            self.heard.pop_front();
        }
        let ms = hyper_rt::futures::now_ns() / 1_000_000;
        self.heard.push_back(format!("{ms} ms: {what}"));
    }

    /// Under the simulation, one more of what the diagnosis counts.
    #[cfg(feature = "sim")]
    fn count(&mut self, what: String) {
        let count = self.counts.entry(what).or_default();
        *count = count.saturating_add(1);
    }

    /// Sends to `node` from now (or, for none, the next node after the one it sent to), and
    /// everything owed goes again.
    fn choose(&mut self, node: Option<u64>, now: Instant) {
        let next = node.or_else(|| {
            let ids: Vec<u64> = self.nodes.iter().map(|(id, _)| *id).collect();
            match self.target {
                None if ids.contains(&self.prefer) => Some(self.prefer),
                None => ids.first().copied(),
                Some(current) => {
                    let at = ids.iter().position(|id| *id == current).unwrap_or(0);
                    at.saturating_add(1)
                        .checked_rem(ids.len())
                        .and_then(|at| ids.get(at))
                        .copied()
                }
            }
        });
        if next != self.target {
            #[cfg(feature = "sim")]
            self.count(format!("chooses {next:?}, was {:?}", self.target));
            self.leads = false;
            self.answered_at = now;
        }
        self.target = next;
        if let Some(target) = next {
            self.dial(target, now);
        }
        self.resend_at = Some(now);
    }

    /// Dials `node`, unless it's connected or being dialed (the transport's own rule); again
    /// after [`RETRY`] if it doesn't connect.
    fn dial(&mut self, node: u64, now: Instant) {
        self.dial_at = now.checked_add(RETRY);
        if let Some((_, address)) = self.nodes.iter().find(|(id, _)| *id == node).copied() {
            let dialed = self.driver.endpoint().connect(now, node, address);
            #[cfg(feature = "sim")]
            self.count(format!("dials {node} at {address}: {dialed:?}"));
            if dialed.is_err() {
                self.stats.dials_refused = self.stats.dials_refused.saturating_add(1);
            }
        }
    }

    /// Under the simulation, what the publisher's endpoint holds: its figures, and the paths it has
    /// established.
    #[cfg(feature = "sim")]
    pub(crate) fn endpoint_state(&mut self) -> String {
        let mut established = Vec::new();
        self.driver.endpoint().established(&mut established);
        format!(
            "endpoint {:?} established {established:?} nodes {:?} connected {:?}",
            self.driver.endpoint().stats(),
            self.nodes,
            self.connected
        )
    }

    /// Acts on one of its endpoint's events; what it settled.
    pub(crate) fn on_event(&mut self, event: Event<Bus>, now: Instant) -> Vec<Settled> {
        #[cfg(feature = "sim")]
        if let Some((peer, up)) = peer_change(&event) {
            self.hear(format!("peer {peer} {}", if up { "up" } else { "down" }));
        }
        #[cfg(feature = "sim")]
        let sender = match &event {
            Event::Frame { peer, .. } => Some(*peer),
            _ => None,
        };
        if let Some((peer, up)) = peer_change(&event) {
            if up {
                self.stats.connects = self.stats.connects.saturating_add(1);
                if !self.connected.contains(&peer) {
                    self.connected.push(peer);
                }
                if Some(peer) == self.target {
                    self.resend_at = Some(now);
                }
            } else {
                self.stats.losses = self.stats.losses.saturating_add(1);
                self.connected.retain(|p| *p != peer);
                if Some(peer) == self.target {
                    // Its connection went: the next node is tried, and it's dialed again later.
                    self.redial.note(peer, false, now);
                    self.choose(None, now);
                }
            }
        }
        let mut settled = Vec::new();
        let mut redirects = Vec::new();
        let producer = self.producer;
        let owed = &mut self.owed;
        let stats = &mut self.stats;
        let mut out_of_order = false;
        let mut acked_by = None;
        let from = self.target.unwrap_or(0);
        Client::new(from).on_event(
            self.driver.endpoint(),
            event,
            now,
            |delivery| match delivery {
                Delivery::Acked(ack) if ack.producer == producer && ack.stream == STREAM => {
                    stats.acks = stats.acks.saturating_add(1);
                    if ack.class == Ack::OUT_OF_ORDER {
                        out_of_order = true;
                    } else if ack.class == Ack::REFUSED {
                        if let Some(gone) = owed.remove(&ack.sequence) {
                            settled.push(Settled::Refused(gone.ticket));
                        }
                    } else {
                        acked_by = Some(from);
                        if let Some(entry) = owed.get_mut(&ack.sequence) {
                            entry.batch = None;
                            if ack.count > 0 {
                                entry.base = Some(ack.base);
                            }
                        }
                    }
                }
                Delivery::Redirected(redirect) if redirect.producer == producer => {
                    stats.redirects = stats.redirects.saturating_add(1);
                    redirects.push(redirect.leader);
                }
                _ => {}
            },
        );
        #[cfg(feature = "sim")]
        for leader in &redirects {
            self.count(format!(
                "redirect from {sender:?} to {:?} in term {}",
                leader.node, leader.term
            ));
        }
        if acked_by.is_some() {
            self.leads = true;
            self.led_at = now;
        }
        if acked_by.is_some() || !redirects.is_empty() {
            self.answered_at = now;
        }
        if out_of_order {
            self.resend_at = Some(now);
        }
        for leader in redirects {
            match leader.node {
                Some(node)
                    if leader.term > self.term
                        || (leader.term == self.term && Some(node) != self.target) =>
                {
                    self.term = leader.term;
                    self.led_at = now;
                    self.choose(Some(node), now);
                    self.leads = true;
                }
                Some(_) => {}
                None => {
                    self.leads = false;
                    self.retry_at = Some(now.checked_add(RETRY).unwrap_or(now));
                }
            }
        }
        settled
    }

    /// The batch of one record for sequence `sequence`.
    fn batch(
        builder: &mut BatchBuilder,
        producer: u64,
        sequence: u32,
        record: &[u8],
        at_us: u64,
    ) -> Result<Vec<u8>, String> {
        builder.begin(&Producing {
            stream: STREAM,
            producer,
            sequence,
            flags: 0,
            timestamp: at_us,
        });
        builder.push(at_us, record).map_err(said)?;
        Ok(builder.finish().map_err(said)?.to_vec())
    }

    /// Sends what's due: everything owed again, where that's due, then what's queued, in order;
    /// what it refused as it built (its ticket and why).
    pub(crate) fn pump(&mut self, now: Instant) -> Vec<(u64, String)> {
        let mut refused = Vec::new();
        self.redial.dial(self.driver.endpoint(), &self.nodes, now);
        if self.retry_at.is_some_and(|at| at <= now) {
            self.retry_at = None;
            self.choose(None, now);
        }
        let waiting = self.owed.values().any(|owed| owed.batch.is_some());
        if waiting && now.saturating_duration_since(self.answered_at) >= SILENT {
            self.choose(None, now);
        }
        let Some(target) = self.target else {
            return refused;
        };
        if !self.connected.contains(&target) {
            // Dialed again each retry until it connects (a dial the transport refused as it was
            // asked ends in no event: a publisher that dialed once waited for good), and left
            // for the next node once it hasn't for as long as an answer may take.
            let pending = !self.queued.is_empty() || !self.owed.is_empty();
            if pending && now.saturating_duration_since(self.answered_at) >= SILENT {
                self.choose(None, now);
            } else if self.dial_at.is_none_or(|at| at <= now) {
                self.dial(target, now);
            }
            return refused;
        }
        let client = Client::new(target);
        if self.resend_at.is_some_and(|at| at <= now) {
            let mut all_sent = true;
            for owed in self.owed.values() {
                let Some(batch) = &owed.batch else {
                    continue;
                };
                match client.publish(self.driver.endpoint(), batch) {
                    Ok(()) => {
                        self.stats.resent = self.stats.resent.saturating_add(1);
                        #[cfg(feature = "sim")]
                        {
                            let count = self
                                .counts
                                .entry(format!("resent to {target}"))
                                .or_default();
                            *count = count.saturating_add(1);
                        }
                    }
                    Err(_) => {
                        all_sent = false;
                        break;
                    }
                }
            }
            if !all_sent {
                // The lane has no room: the rest go when it has (`LaneRoom` brings a pump).
                return refused;
            }
            self.resend_at = None;
        }
        while let Some(queued) = self.queued.front() {
            let sequence = self.next;
            let built = Self::batch(
                &mut self.builder,
                self.producer,
                sequence,
                &queued.record,
                queued.at_us,
            );
            let batch = match built {
                Ok(batch) => batch,
                Err(why) => {
                    if let Some(gone) = self.queued.pop_front() {
                        refused.push((gone.ticket, why));
                    }
                    continue;
                }
            };
            match client.publish(self.driver.endpoint(), &batch) {
                Ok(()) => {}
                Err(WireRefusal::LaneFull | WireRefusal::NotConnected) => break,
                Err(other) => {
                    if let Some(gone) = self.queued.pop_front() {
                        refused.push((gone.ticket, format!("the transport refused it: {other:?}")));
                    }
                    continue;
                }
            }
            let Some(next) = self.next.checked_add(1) else {
                // A session's sequences are spent: the next batch would repeat one the part knows.
                if let Some(gone) = self.queued.pop_front() {
                    refused.push((gone.ticket, "the publisher's session is spent".to_owned()));
                }
                break;
            };
            if let Some(sent) = self.queued.pop_front() {
                self.owed.insert(
                    sequence,
                    Owed {
                        batch: Some(batch),
                        ticket: sent.ticket,
                        base: None,
                    },
                );
                self.next = next;
                self.stats.sent = self.stats.sent.saturating_add(1);
            }
        }
        if !self.owed.is_empty() && self.resend_at.is_none() {
            self.resend_at = now.checked_add(RESEND);
        }
        refused
    }

    /// The tickets of publishes never sent, asked `wait` ago or longer, with no leader known
    /// for as long: refused, for no quorum.
    pub(crate) fn expire(&mut self, wait: Duration, now: Instant) -> Vec<u64> {
        let leaderless = !self.leads && now.saturating_duration_since(self.led_at) >= wait;
        let mut gone = Vec::new();
        while let Some(front) = self.queued.front() {
            if !(leaderless || !self.has_nodes())
                || now.saturating_duration_since(front.asked) < wait
            {
                break;
            }
            if let Some(expired) = self.queued.pop_front() {
                gone.push(expired.ticket);
            }
        }
        gone
    }

    /// The ticket of the batch of `sequence` the part took, if this publisher owed it.
    pub(crate) fn committed(&mut self, sequence: u32) -> Option<u64> {
        self.owed.remove(&sequence).map(|owed| owed.ticket)
    }

    /// The tickets of batches acknowledged at offsets below `floor`, and those offsets: the part
    /// this node reads no longer holds them, so its feed won't show them.
    pub(crate) fn below(&mut self, floor: u64) -> Vec<(u64, u64)> {
        let gone: Vec<u32> = self
            .owed
            .iter()
            .filter(|(_, owed)| owed.base.is_some_and(|base| base < floor))
            .map(|(sequence, _)| *sequence)
            .collect();
        gone.into_iter()
            .filter_map(|sequence| {
                let owed = self.owed.remove(&sequence)?;
                Some((owed.ticket, owed.base?))
            })
            .collect()
    }

    /// Every ticket it holds, sent or not, as it goes.
    pub(crate) fn tickets(&self) -> Vec<u64> {
        self.owed
            .values()
            .map(|owed| owed.ticket)
            .chain(self.queued.iter().map(|queued| queued.ticket))
            .collect()
    }

    /// The node it sends to, and the batches it owes and holds unsent.
    /// Under the simulation: the node it sends to, whether it knows that node leads, and the
    /// term it knows.
    #[cfg(feature = "sim")]
    pub(crate) const fn known(&self) -> (Option<u64>, bool, u64) {
        (self.target, self.leads, self.term)
    }

    pub(crate) fn state(&self) -> (Option<u64>, usize, usize) {
        (self.target, self.owed.len(), self.queued.len())
    }

    pub(crate) fn deadline(&self, wait: Duration) -> Option<Instant> {
        let expiry = self.queued.front().and_then(|q| q.asked.checked_add(wait));
        let dial = self
            .target
            .filter(|target| !self.connected.contains(target))
            .and(self.dial_at);
        [
            self.resend_at,
            self.retry_at,
            dial,
            self.redial.deadline(),
            expiry,
        ]
        .into_iter()
        .flatten()
        .min()
    }
}

/// What a feed gives: a committed batch, or the offset the part keeps from where it kept nothing
/// of what the feed had yet to give.
pub(crate) enum Fed<'a> {
    Batch(FocalBatch<'a>),
    Skip(u64),
}

/// Where a node is in what its member's part took (module docs): the next offset to give, and the
/// position of the batch it gave last, the one whose link leads to the next.
#[derive(Debug, Default)]
pub(crate) struct Cursor {
    pub(crate) next: u64,
    at: Option<u64>,
}

impl Cursor {
    /// Gives `each` what `part` took past what this cursor gave, in order, at most
    /// [`FEED_MOST`] batches; whether it stopped at that bound with more to give.
    pub(crate) fn feed(&mut self, part: &Part, mut each: impl FnMut(Fed<'_>)) -> bool {
        let Some((floor, end)) = part.bounds(STREAM) else {
            return false;
        };
        if self.next < floor {
            each(Fed::Skip(floor));
            self.next = floor;
            self.at = None;
        }
        if self.next >= end {
            return false;
        }
        // The batch holding `next`: after the one given last where it's still held, else found
        // from the stream's first.
        let after_last = self
            .at
            .and_then(|word| part.batch(Position::from_word(word)))
            .filter(|last| last.stream() == STREAM && last.end_offset() == self.next)
            .map(|last| last.next());
        let mut word = match after_last {
            Some(Some(word)) => Some(word),
            // The last given has no next yet: nothing more is linked.
            Some(None) => return false,
            None => self.find(part),
        };
        let mut given = 0_usize;
        while let Some(at) = word {
            if given >= FEED_MOST {
                return true;
            }
            given = given.saturating_add(1);
            let Some(batch) = part.batch(Position::from_word(at)) else {
                return false;
            };
            word = batch.next();
            if batch.end_offset() <= self.next {
                continue;
            }
            self.next = batch.end_offset();
            self.at = Some(at);
            each(Fed::Batch(batch));
        }
        false
    }

    /// The position of the batch holding `next`, walking the stream from its first batch: bounded
    /// by the batches the part holds.
    fn find(&self, part: &Part) -> Option<u64> {
        let (floor, end) = part.bounds(STREAM)?;
        let mut word = part.first_word(STREAM)?;
        // Each batch holds a record at least, so the stream holds at most `end - floor` of them.
        for _ in floor..end {
            let batch = part.batch(Position::from_word(word))?;
            if batch.end_offset() > self.next {
                return Some(word);
            }
            word = batch.next()?;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use focal_bus::batch::{BatchBuilder, HEADER};
    use focal_bus::wire::BATCH_BOUND;
    use quiver_api::{BATCH_FRAMING_MAX, Class, KEY_MAX};

    use super::Publisher;
    use crate::ephemeral::record_into;

    /// QUIVER-9 for the persisted class: quiver-api's framing bound is Focal's at the most (its
    /// header, a record's longest head, Quiver's key framing and the longest key), so do frame the
    /// longest key with the class's largest payload, expect a batch Focal takes whole.
    #[test]
    fn the_largest_persisted_message_fits_focals_largest_batch() {
        assert_eq!(
            BATCH_FRAMING_MAX,
            HEADER + BatchBuilder::record_bytes(0) + 2 + KEY_MAX
        );
        let mut record = Vec::new();
        record_into(
            &mut record,
            Some(&[1; KEY_MAX]),
            &vec![2; Class::Persisted.payload_max()],
        );
        let batch = Publisher::batch(&mut BatchBuilder::new(), 1, 0, &record, 1).unwrap();
        assert!(
            batch.len() <= usize::try_from(BATCH_BOUND).unwrap(),
            "{}",
            batch.len()
        );
    }
}
