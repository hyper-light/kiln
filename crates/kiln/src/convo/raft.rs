//! Per-session/conversation Raft replication (`proto::feature::CONVO_RAFT`): every agent session
//! owns an independent `hyper-raft` consensus group whose **initial leader is the node where the
//! session starts and runs** (never necessarily the node whose UI opened it — e.g. opening the UI
//! on `laptop-a:7070` and starting a session on `laptop-b` elects `laptop-b` as that
//! conversation's Raft leader).
//!
//! All conversation writes (`Row::Session`, `Event`, `Reset`, `Closed`, `Record`, `Machine`,
//! `Inbox`) proposed on the owning leader commit through `hyper-raft` and replicate across every
//! connected node in the fleet (over direct P2P QUIC links first, falling back to relayed QUIC on
//! `LANE_REACH` and `LANE_CONVO`). Followers/learners apply committed entries straight into their
//! local [`super::store::Store`], so every node's `localhost:7070` acts as a live, unified
//! entrypoint into the fleet's conversations rather than a siloed island.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::task::Waker;
use std::time::{Duration, Instant};

use hyper_durable::{EntryRef, Fatal, Output, Point, RamStore, Replica, Settings, StateMachine, Unbounded};
use hyper_raft::proto::{ConfChangeSingle, ConfChangeTransition, ConfChangeType, ConfChangeV2, ConfState, EntryType, Message};
use hyper_raft::wire::Record as _;
use hyper_raft::{Config, Elections, Limits, Stated};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::rows::Row;
use super::store::Store;

/// Stable Raft member id for the central server node (`1`, matching `store::group::SERVER`).
pub const SERVER_ID: u64 = 1;
/// Largest single Raft message carried in one frame.
const MESSAGE_MAX: usize = 768 << 10;
/// Largest entries payload per Raft append message.
const APPEND_MAX: u64 = 256 << 10;
/// Maximum peers tracked per conversation Raft group.
const MAX_MEMBERS: usize = 64;
/// Heartbeat interval in ticks (driven by the manager's tick loop).
const BEAT_TICKS: usize = 5;
/// Tick cadence for driving per-session Raft groups.
pub const TICK: Duration = Duration::from_millis(200);
/// Maximum in-memory log entries kept before compacting behind durable state in `Store`.
const LOG_KEEP: u64 = 256;
/// Maximum outgoing Raft messages queued to the network transport before shedding (Raft resends).
const OUTGOING_CAP: usize = 4096;
/// Maximum number of concurrent per-session Raft groups kept active in memory on one node.
const MAX_GROUPS: usize = 1024;

/// Deterministic Raft node ID for a kiln node name (`"server"` -> `1`, every runner -> `>= 2`),
/// matching `crate::store::group::member_of` so every node in the fleet computes the exact same id.
pub fn node_id(name: &str) -> u64 {
    if name == "server" {
        return SERVER_ID;
    }
    crate::store::group::member_of(name)
}

/// One entry committed to a conversation's Raft log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConvoEntry {
    pub session: String,
    pub leader: String,
    #[serde(default)]
    pub epoch: u64,
    pub rows: Vec<Row>,
}

/// Snapshot payload transferred when a late-joining peer is behind the compacted Raft log.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct ConvoImage {
    session: String,
    leader: String,
    epoch: u64,
    rows: Vec<Row>,
}

/// An outgoing Raft message produced by a session's Raft group for a specific peer node.
#[derive(Debug, Clone)]
pub struct Outgoing {
    pub session: String,
    pub to_id: u64,
    pub to_name: String,
    pub bytes: Vec<u8>,
}

/// Public view of one session's Raft group (exposed on `/healthz/server` and `/api/convo/raft`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupStatus {
    pub session: String,
    pub node: String,
    pub node_id: u64,
    pub is_leader: bool,
    pub leader_id: u64,
    pub leader_name: Option<String>,
    pub term: u64,
    pub applied: u64,
    pub epoch: u64,
    pub voters: Vec<u64>,
    pub learners: Vec<u64>,
}

/// Per-session state machine backed by this node's local Turso conversation [`Store`].
struct ConvoMachine {
    session: String,
    local_node: String,
    leader_name: String,
    epoch: u64,
    store: Option<Store>,
    applied: Point,
    configuration: ConfState,
    /// Recent rows kept for fast snapshot generation if no backing `Store` is attached (or in tests).
    recent: Vec<Row>,
}

impl ConvoMachine {
    fn new(session: String, local_node: String, leader_name: String, initial_conf: ConfState, store: Option<Store>) -> Self {
        Self { session, local_node, leader_name, epoch: 0, store, applied: Point::default(), configuration: initial_conf, recent: Vec::new() }
    }

    fn apply_rows(&mut self, entry: &ConvoEntry) {
        if entry.epoch < self.epoch {
            return;
        }
        self.epoch = entry.epoch;
        if !entry.leader.is_empty() {
            self.leader_name = entry.leader.clone();
        }
        if entry.rows.is_empty() {
            return;
        }
        // Keep bounded snapshot tail for late-join catchup even without reading back from SQLite.
        self.recent.extend(entry.rows.iter().cloned());
        if self.recent.len() > 256 {
            let drain = self.recent.len().saturating_sub(256);
            self.recent.drain(..drain);
        }
        // Only followers/learners write replicated rows into their local store via Raft apply
        // (the owning leader's local harness tee already wrote its own rows into its own store,
        // avoiding duplicate write amplification on the leader).
        if self.leader_name != self.local_node
            && let Some(store) = &self.store
        {
            let mut rows = entry.rows.clone();
            if !rows.iter().any(|r| matches!(r, Row::Machine { .. })) && !self.leader_name.is_empty() {
                rows.push(Row::Machine { session: self.session.clone(), machine: self.leader_name.clone() });
            }
            store.put(rows);
        }
    }
}

impl StateMachine for ConvoMachine {
    type Answer = ();

    fn apply(&mut self, entry: &EntryRef<'_>, _answers: &mut Vec<()>) -> Result<(), Fatal> {
        self.applied = Point { index: entry.index, term: entry.term };
        if entry.kind != EntryType::EntryNormal || entry.data.is_empty() {
            return Ok(());
        }
        if let Ok(ce) = serde_json::from_slice::<ConvoEntry>(entry.data) {
            self.apply_rows(&ce);
        }
        Ok(())
    }

    fn apply_change(&mut self, at: Point, _change: &ConfChangeV2, configuration: &ConfState) -> Result<(), Fatal> {
        self.applied = at;
        self.configuration = configuration.clone();
        Ok(())
    }

    fn durable(&self) -> Point {
        self.applied
    }

    fn configuration(&self) -> &ConfState {
        &self.configuration
    }

    fn acts_at_start(&self, _entry: &EntryRef<'_>) -> bool {
        false
    }

    fn image(&mut self, into: &mut Vec<u8>) -> Result<(Point, ConfState), Fatal> {
        let mut rows = self.recent.clone();
        if rows.is_empty()
            && let Some(store) = &self.store
            && let Ok(Some(kept)) = store.kept_committed(&self.session, 128)
        {
            rows.push(Row::Session {
                session: self.session.clone(),
                info: kept.info,
                meta: kept.meta,
                state: kept.state,
                changes: kept.changes,
                epoch: Some(self.epoch),
                open: kept.closed_at.is_none(),
                at: kept.updated_at,
            });
            for ev in kept.events {
                rows.push(Row::Event(Box::new(ev)));
            }
            if !self.leader_name.is_empty() {
                rows.push(Row::Machine { session: self.session.clone(), machine: self.leader_name.clone() });
            }
        }
        let img = ConvoImage { session: self.session.clone(), leader: self.leader_name.clone(), epoch: self.epoch, rows };
        let encoded = serde_json::to_vec(&img).map_err(|_| Fatal("conversation Raft snapshot failed to encode"))?;
        into.extend_from_slice(&encoded);
        Ok((self.applied, self.configuration.clone()))
    }

    fn image_bytes(&self) -> Option<u64> {
        Some(u64::try_from(self.recent.iter().map(Row::bytes).sum::<usize>()).unwrap_or(0))
    }

    fn install(&mut self, image: &[u8], at: Point, configuration: &ConfState) -> Result<(), Fatal> {
        let img: ConvoImage = serde_json::from_slice(image).map_err(|_| Fatal("conversation Raft snapshot failed to decode"))?;
        self.applied = at;
        self.configuration = configuration.clone();
        self.epoch = self.epoch.max(img.epoch);
        if !img.leader.is_empty() {
            self.leader_name = img.leader.clone();
        }
        let entry = ConvoEntry { session: self.session.clone(), leader: self.leader_name.clone(), epoch: self.epoch, rows: img.rows };
        self.apply_rows(&entry);
        Ok(())
    }

    fn persist(&mut self) -> Result<(), Fatal> {
        Ok(())
    }
}

fn raft_settings(id: u64) -> anyhow::Result<Settings> {
    let limits = Limits::derive(Stated { message: MESSAGE_MAX, members: MAX_MEMBERS, memory: 16 << 20, depth: 1 })
        .map_err(|e| anyhow::anyhow!("conversation Raft limits: {e:?}"))?;
    Ok(Settings {
        core: Config { election_tick: BEAT_TICKS * 3, heartbeat_tick: BEAT_TICKS, max_size_per_msg: APPEND_MAX, pre_vote: true, check_quorum: true, seed: id, ..Config::new(id, limits) },
        elections: Elections::Ticks,
        quiet: Duration::ZERO,
        disk: hyper_durable::Disk::Flushed,
    })
}

fn conf_change(member: u64, change_type: ConfChangeType) -> ConfChangeV2 {
    ConfChangeV2 { transition: ConfChangeTransition::Auto, changes: vec![ConfChangeSingle { change_type, node_id: member }], context: Vec::new() }
}

type SessionReplica = Replica<RamStore, ConvoMachine, Unbounded>;

struct SessionGroup {
    replica: SessionReplica,
    /// Members we want replicated as learners/voters on this session (maintained by the leader).
    wanted_learners: BTreeSet<u64>,
    /// Last time a conf change was proposed (avoid flooding while one is in flight).
    changing: Option<(u64, Instant)>,
    /// Last activity timestamp (for LRU eviction past `MAX_GROUPS`).
    touched: Instant,
}

struct Inner {
    me: String,
    my_id: u64,
    store: Option<Store>,
    born: Instant,
    /// Known fleet peers: Raft node id -> node name (`"server"`, runner name/id).
    peers: BTreeMap<u64, String>,
    /// Active per-session Raft groups keyed by session id.
    groups: HashMap<String, SessionGroup>,
    /// Session id -> last known leader node name (even after local group eviction).
    leaders: HashMap<String, String>,
    /// Pending conf-change transfer targets (`session -> new_leader_id`).
    transfers: HashMap<String, u64>,
}

/// Manages all per-session/conversation `hyper-raft` groups on this node.
pub struct Manager {
    inner: Mutex<Inner>,
    out_tx: crate::memory::queue::Sender<Outgoing>,
    out_rx: crate::memory::queue::Receiver<Outgoing>,
}

static GLOBAL: arc_swap::ArcSwapOption<Manager> = arc_swap::ArcSwapOption::const_empty();

/// Publish this node's conversation Raft manager globally so `frontdoor::replica::Door` and
/// `/healthz/server` can inspect per-session leadership and route/read cleanly.
pub fn set_global(manager: Option<Arc<Manager>>) {
    GLOBAL.store(manager);
}

/// This node's active conversation Raft manager, if started.
pub fn global() -> Option<Arc<Manager>> {
    GLOBAL.load_full()
}

impl Manager {
    /// Create a conversation Raft manager for node `me` backed by its local conversation `store`.
    pub fn new(me: impl Into<String>, store: Option<Store>) -> Arc<Self> {
        let me = me.into();
        let my_id = node_id(&me);
        let (out_tx, out_rx) = crate::memory::queue::unbounded(&crate::memory::owners::CHANNELS);
        let mut peers = BTreeMap::new();
        peers.insert(my_id, me.clone());
        Arc::new(Self {
            inner: Mutex::new(Inner { me, my_id, store, born: Instant::now(), peers, groups: HashMap::new(), leaders: HashMap::new(), transfers: HashMap::new() }),
            out_tx,
            out_rx,
        })
    }

    /// This node's name and Raft member id.
    pub fn identity(&self) -> (String, u64) {
        let g = self.inner.lock();
        (g.me.clone(), g.my_id)
    }

    /// Attach or replace the backing conversation store (e.g. after deferred handover store open).
    pub fn set_store(&self, store: Store) {
        let mut g = self.inner.lock();
        g.store = Some(store.clone());
        for sg in g.groups.values_mut() {
            sg.replica.machine_mut().store = Some(store.clone());
        }
    }

    /// Outgoing Raft frames ready to be delivered to peer nodes (over direct QUIC or relayed QUIC).
    pub fn outgoing(&self) -> crate::memory::queue::Receiver<Outgoing> {
        self.out_rx.clone()
    }

    /// Register connected peer nodes in the fleet; any session led by THIS node adds them as Raft
    /// learners so conversations replicate automatically across all connected nodes.
    pub fn register_peers<I, S>(&self, peer_names: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut out = Vec::new();
        {
            let mut g = self.inner.lock();
            let my_id = g.my_id;
            let mut added_ids = Vec::new();
            for name in peer_names {
                let name = name.into();
                if name.is_empty() || name == g.me {
                    continue;
                }
                let id = node_id(&name);
                if g.peers.insert(id, name).is_none() {
                    added_ids.push(id);
                }
            }
            let all_peer_ids: Vec<u64> = g.peers.keys().copied().filter(|id| *id != my_id).collect();
            let peers_snapshot = g.peers.clone();
            let now = g.now_ns();
            let sessions: Vec<String> = g.groups.keys().cloned().collect();
            for session in sessions {
                let Some(sg) = g.groups.get_mut(&session) else { continue };
                if !sg.replica.is_leader() {
                    continue;
                }
                for pid in &all_peer_ids {
                    sg.wanted_learners.insert(*pid);
                }
                reconcile_learners_and_drive(&session, sg, now, &peers_snapshot, &mut out);
            }
        }
        self.emit(out);
    }

    /// Mark a peer temporarily unreachable in every active Raft group.
    pub fn peer_unreachable(&self, peer_name: &str) {
        let pid = node_id(peer_name);
        let mut g = self.inner.lock();
        for sg in g.groups.values_mut() {
            let _ = sg.replica.report_unreachable(pid);
        }
    }

    /// Start (or take leadership of) `session`'s Raft group on THIS node — called on the runner
    /// where the session is actually started/opened. Elects THIS node as the session's initial
    /// Raft leader and replicates to all known connected fleet peers.
    pub fn open_leader(&self, session: &str, epoch: u64, initial_rows: Vec<Row>) -> anyhow::Result<GroupStatus> {
        let mut out = Vec::new();
        let status = {
            let mut g = self.inner.lock();
            g.evict_if_full();
            let me = g.me.clone();
            let my_id = g.my_id;
            let now = g.now_ns();
            let peer_ids: BTreeSet<u64> = g.peers.keys().copied().filter(|id| *id != my_id).collect();
            let store = g.store.clone();
            let need_create = !g.groups.contains_key(session);
            if need_create {
                let conf = ConfState { voters: vec![my_id], ..ConfState::default() };
                let machine = ConvoMachine::new(session.to_owned(), me.clone(), me.clone(), conf, store);
                let mut replica = Replica::open(&raft_settings(my_id)?, RamStore::new(), machine, Unbounded)
                    .map_err(|e| anyhow::anyhow!("opening leader Raft group for {session}: {e:?}"))?;
                replica.campaign().map_err(|e| anyhow::anyhow!("campaigning for {session}: {e:?}"))?;
                let mut sg = SessionGroup { replica, wanted_learners: peer_ids, changing: None, touched: Instant::now() };
                // Sole initial voter elects itself synchronously in one drive pass.
                drive_group(session, &mut sg, now, &g.peers, &mut out);
                g.groups.insert(session.to_owned(), sg);
            }
            let peers_snapshot = g.peers.clone();
            let Some(sg) = g.groups.get_mut(session) else {
                anyhow::bail!("session Raft group {session} missing after open");
            };
            sg.touched = Instant::now();
            if !sg.replica.is_leader() && sg.replica.configuration().voters.contains(&my_id) {
                let _ = sg.replica.campaign();
                drive_group(session, sg, now, &peers_snapshot, &mut out);
            }
            if !initial_rows.is_empty() && sg.replica.is_leader() {
                let ce = ConvoEntry { session: session.to_owned(), leader: me.clone(), epoch, rows: initial_rows };
                if let Ok(bytes) = serde_json::to_vec(&ce) {
                    let _ = sg.replica.propose(Vec::new(), bytes);
                }
            }
            reconcile_learners_and_drive(session, sg, now, &peers_snapshot, &mut out);
            let st = group_status(session, &me, my_id, sg, &peers_snapshot);
            if let Some(ln) = &st.leader_name {
                g.leaders.insert(session.to_owned(), ln.clone());
            }
            st
        };
        self.emit(out);
        Ok(status)
    }

    /// Propose new conversation rows for `session` on its leader. If this node is not yet holding
    /// the group (e.g. a restored session), automatically opens it as leader on this node.
    pub fn propose(&self, session: &str, epoch: u64, rows: Vec<Row>) -> bool {
        if rows.is_empty() {
            return true;
        }
        let mut out = Vec::new();
        let proposed = {
            let mut g = self.inner.lock();
            let me = g.me.clone();
            let my_id = g.my_id;
            let now = g.now_ns();
            if !g.groups.contains_key(session) {
                g.evict_if_full();
                let peer_ids: BTreeSet<u64> = g.peers.keys().copied().filter(|id| *id != my_id).collect();
                let conf = ConfState { voters: vec![my_id], ..ConfState::default() };
                let machine = ConvoMachine::new(session.to_owned(), me.clone(), me.clone(), conf, g.store.clone());
                if let Ok(settings) = raft_settings(my_id)
                    && let Ok(mut replica) = Replica::open(&settings, RamStore::new(), machine, Unbounded)
                {
                    let _ = replica.campaign();
                    let mut sg = SessionGroup { replica, wanted_learners: peer_ids, changing: None, touched: Instant::now() };
                    drive_group(session, &mut sg, now, &g.peers, &mut out);
                    g.groups.insert(session.to_owned(), sg);
                }
            }
            let peers_snapshot = g.peers.clone();
            let Some(sg) = g.groups.get_mut(session) else { return false };
            sg.touched = Instant::now();
            if !sg.replica.is_leader() {
                return false;
            }
            let ce = ConvoEntry { session: session.to_owned(), leader: me.clone(), epoch, rows };
            let ok = serde_json::to_vec(&ce).ok().is_some_and(|b| sg.replica.propose(Vec::new(), b).is_ok());
            reconcile_learners_and_drive(session, sg, now, &peers_snapshot, &mut out);
            // Compact behind `LOG_KEEP` so long-running sessions stay bounded in RAM.
            let applied = sg.replica.applied().index;
            if applied > LOG_KEEP * 2 {
                let _ = sg.replica.compact(LOG_KEEP, now, Waker::noop());
            }
            g.leaders.insert(session.to_owned(), me);
            ok
        };
        self.emit(out);
        proposed
    }

    /// Initiate Raft leadership transfer for `session` to `target_node` (e.g. when a session moves
    /// from `laptop-b` to `laptop-c`). Promotes `target_node` to voter first if it is currently a
    /// learner or absent, then transfers Raft leadership once committed.
    pub fn transfer_leadership(&self, session: &str, target_node: &str) -> bool {
        let target_id = node_id(target_node);
        let mut out = Vec::new();
        let ok = {
            let mut g = self.inner.lock();
            if target_id == g.my_id {
                return true;
            }
            g.peers.insert(target_id, target_node.to_owned());
            let peers_snapshot = g.peers.clone();
            let now = g.now_ns();
            let Some(sg) = g.groups.get_mut(session) else { return false };
            if !sg.replica.is_leader() {
                return false;
            }
            sg.touched = Instant::now();
            let conf = sg.replica.configuration();
            if conf.voters.contains(&target_id) {
                let _ = sg.replica.transfer(target_id);
                drive_group(session, sg, now, &peers_snapshot, &mut out);
                g.transfers.remove(session);
                true
            } else {
                // Promote target to voter first; tick/step finishes the leadership transfer once applied.
                let change = conf_change(target_id, ConfChangeType::AddNode);
                let proposed = sg.replica.change(Vec::new(), &change).is_ok();
                if proposed {
                    sg.changing = Some((target_id, Instant::now()));
                    drive_group(session, sg, now, &peers_snapshot, &mut out);
                    g.transfers.insert(session.to_owned(), target_id);
                }
                proposed
            }
        };
        self.emit(out);
        ok
    }

    /// Deliver an incoming Raft message for `session` from `from_node`. If this node does not yet
    /// have a local follower/learner replica for `session`, one is instantiated on demand with the
    /// sender as initial configuration voter so Raft replication starts automatically on first contact.
    pub fn step(&self, from_node: &str, session: &str, bytes: &[u8]) -> anyhow::Result<()> {
        let msg = Message::decode(bytes).map_err(|e| anyhow::anyhow!("decode convo Raft message for {session}: {e:?}"))?;
        let mut out = Vec::new();
        {
            let mut g = self.inner.lock();
            let from_id = msg.from;
            if from_id != 0 && !from_node.is_empty() {
                g.peers.insert(from_id, from_node.to_owned());
            }
            let me = g.me.clone();
            let my_id = g.my_id;
            let now = g.now_ns();
            if !g.groups.contains_key(session) {
                g.evict_if_full();
                let leader_id = if from_id != 0 && from_id != my_id { from_id } else { node_id(from_node) };
                let conf = ConfState { voters: vec![leader_id], learners: vec![my_id], ..ConfState::default() };
                let machine = ConvoMachine::new(session.to_owned(), me.clone(), from_node.to_owned(), conf, g.store.clone());
                let replica = Replica::open(&raft_settings(my_id)?, RamStore::new(), machine, Unbounded)
                    .map_err(|e| anyhow::anyhow!("opening follower Raft group for {session}: {e:?}"))?;
                g.groups.insert(session.to_owned(), SessionGroup { replica, wanted_learners: BTreeSet::new(), changing: None, touched: Instant::now() });
            }
            let peers_snapshot = g.peers.clone();
            let pending_transfer = g.transfers.get(session).copied();
            if let Some(sg) = g.groups.get_mut(session) {
                sg.touched = Instant::now();
                if let Err(e) = sg.replica.step(msg) {
                    tracing::debug!(session, from = %from_node, "convo Raft step refused: {e:?}");
                }
                if sg.replica.is_leader() {
                    reconcile_learners(sg);
                    if let Some(target_id) = pending_transfer
                        && sg.replica.configuration().voters.contains(&target_id)
                    {
                        let _ = sg.replica.transfer(target_id);
                    }
                }
                drive_group(session, sg, now, &peers_snapshot, &mut out);
                let leader_id = sg.replica.leader();
                let leader_name = peers_snapshot.get(&leader_id).cloned().or_else(|| {
                    let ln = &sg.replica.machine().leader_name;
                    (!ln.is_empty()).then(|| ln.clone())
                });
                let done_transfer = pending_transfer.is_some_and(|t| leader_id == t || !sg.replica.is_leader());
                if done_transfer {
                    g.transfers.remove(session);
                }
                if let Some(ln) = leader_name {
                    g.leaders.insert(session.to_owned(), ln);
                }
            }
        }
        self.emit(out);
        Ok(())
    }

    /// Drive one Raft tick across all active per-session groups (heartbeats, election timers,
    /// learner catch-up, and pending leadership transfers).
    pub fn tick(&self) {
        let mut out = Vec::new();
        {
            let mut g = self.inner.lock();
            let now = g.now_ns();
            let peers_snapshot = g.peers.clone();
            let transfers = g.transfers.clone();
            let mut finished_transfers = Vec::new();
            let mut leader_updates = Vec::new();
            for (session, sg) in &mut g.groups {
                if sg.replica.is_leader() {
                    reconcile_learners(sg);
                    if let Some(&target_id) = transfers.get(session)
                        && sg.replica.configuration().voters.contains(&target_id)
                    {
                        let _ = sg.replica.transfer(target_id);
                    }
                    let _ = sg.replica.beat();
                }
                drive_group(session, sg, now, &peers_snapshot, &mut out);
                let lid = sg.replica.leader();
                if transfers.get(session).is_some_and(|&t| lid == t || !sg.replica.is_leader()) {
                    finished_transfers.push(session.clone());
                }
                if let Some(ln) = peers_snapshot.get(&lid) {
                    leader_updates.push((session.clone(), ln.clone()));
                }
            }
            for s in finished_transfers {
                g.transfers.remove(&s);
            }
            for (s, ln) in leader_updates {
                g.leaders.insert(s, ln);
            }
        }
        self.emit(out);
    }

    /// Known Raft leader node name for `session`, if any.
    pub fn leader_of(&self, session: &str) -> Option<String> {
        let g = self.inner.lock();
        if let Some(sg) = g.groups.get(session) {
            let lid = sg.replica.leader();
            if let Some(name) = g.peers.get(&lid) {
                return Some(name.clone());
            }
            if !sg.replica.machine().leader_name.is_empty() {
                return Some(sg.replica.machine().leader_name.clone());
            }
        }
        g.leaders.get(session).cloned()
    }

    /// Detailed Raft status for one `session`.
    pub fn status(&self, session: &str) -> Option<GroupStatus> {
        let g = self.inner.lock();
        let sg = g.groups.get(session)?;
        Some(group_status(session, &g.me, g.my_id, sg, &g.peers))
    }

    /// Detailed Raft status for every active session on this node (sorted by session id).
    pub fn all_status(&self) -> Vec<GroupStatus> {
        let g = self.inner.lock();
        let mut out: Vec<GroupStatus> = g.groups.iter().map(|(s, sg)| group_status(s, &g.me, g.my_id, sg, &g.peers)).collect();
        out.sort_by(|a, b| a.session.cmp(&b.session));
        out
    }

    /// Summary counts `(total_groups, leading_groups, following_groups)` for `/healthz/server`.
    pub fn summary(&self) -> (usize, usize, usize) {
        let g = self.inner.lock();
        let total = g.groups.len();
        let leading = g.groups.values().filter(|sg| sg.replica.is_leader()).count();
        (total, leading, total.saturating_sub(leading))
    }

    /// Backing local conversation store (holds both locally led sessions and Raft-replicated peer sessions).
    pub fn store(&self) -> Option<Store> {
        self.inner.lock().store.clone()
    }

    fn emit(&self, msgs: Vec<Outgoing>) {
        for m in msgs {
            let _ = crate::memory::send_within(&self.out_tx, OUTGOING_CAP, || m);
        }
    }
}

impl Inner {
    fn now_ns(&self) -> u64 {
        u64::try_from(self.born.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    fn evict_if_full(&mut self) {
        if self.groups.len() < MAX_GROUPS {
            return;
        }
        // Prefer evicting the stalest follower group first so locally led sessions keep replicating.
        let victim = self
            .groups
            .iter()
            .filter(|(_, sg)| !sg.replica.is_leader())
            .min_by_key(|(_, sg)| sg.touched)
            .or_else(|| self.groups.iter().min_by_key(|(_, sg)| sg.touched))
            .map(|(k, _)| k.clone());
        if let Some(k) = victim {
            self.groups.remove(&k);
        }
    }
}

fn reconcile_learners_and_drive(session: &str, sg: &mut SessionGroup, now: u64, peers: &BTreeMap<u64, String>, out: &mut Vec<Outgoing>) {
    drive_group(session, sg, now, peers, out);
    let mut added = false;
    for _ in 0..MAX_MEMBERS {
        if !reconcile_learners(sg) {
            break;
        }
        added = true;
        drive_group(session, sg, now, peers, out);
    }
    if added && sg.replica.is_leader() {
        let _ = sg.replica.beat();
        drive_group(session, sg, now, peers, out);
    }
}

fn reconcile_learners(sg: &mut SessionGroup) -> bool {
    if !sg.replica.is_leader() {
        return false;
    }
    let conf = sg.replica.configuration();
    if let Some((member, at)) = sg.changing {
        let present = conf.learners.contains(&member) || conf.voters.contains(&member);
        if !present && at.elapsed() < Duration::from_secs(5) {
            return false;
        }
        sg.changing = None;
    }
    let next = sg.wanted_learners.iter().copied().find(|m| !conf.learners.contains(m) && !conf.voters.contains(m));
    if let Some(member) = next
        && sg.replica.change(Vec::new(), &conf_change(member, ConfChangeType::AddLearnerNode)).is_ok()
    {
        sg.changing = Some((member, Instant::now()));
        return true;
    }
    false
}

fn drive_group(session: &str, sg: &mut SessionGroup, now: u64, peers: &BTreeMap<u64, String>, out: &mut Vec<Outgoing>) {
    let mut output: Output<()> = Output::default();
    let waker = Waker::noop();
    for _ in 0..128 {
        output.clear();
        let Ok(driven) = sg.replica.drive(now, waker, &mut output) else {
            break;
        };
        for m in output.messages.drain(..) {
            let to_id = m.to;
            if let Some(to_name) = peers.get(&to_id) {
                out.push(Outgoing { session: session.to_owned(), to_id, to_name: to_name.clone(), bytes: m.encode_to_vec() });
            }
        }
        if !driven.more && driven.out == 0 {
            break;
        }
    }
}

fn group_status(session: &str, me: &str, my_id: u64, sg: &SessionGroup, peers: &BTreeMap<u64, String>) -> GroupStatus {
    let conf = sg.replica.configuration();
    let leader_id = sg.replica.leader();
    let leader_name = peers.get(&leader_id).cloned().or_else(|| {
        let ln = &sg.replica.machine().leader_name;
        (!ln.is_empty()).then(|| ln.clone())
    });
    GroupStatus {
        session: session.to_owned(),
        node: me.to_owned(),
        node_id: my_id,
        is_leader: sg.replica.is_leader(),
        leader_id,
        leader_name,
        term: sg.replica.term(),
        applied: sg.replica.applied().index,
        epoch: sg.replica.machine().epoch,
        voters: conf.voters.clone(),
        learners: conf.learners.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convo::rows::EventRow;

    /// In-memory multi-node fleet simulator routing per-session Raft messages directly between
    /// nodes (e.g. `laptop-a`, `laptop-b`, `laptop-c`, `server`), each with its own Turso store.
    struct Fleet {
        nodes: BTreeMap<String, (Arc<Manager>, Store, tempfile::TempDir)>,
    }

    impl Fleet {
        fn new(names: &[&str]) -> anyhow::Result<Self> {
            let mut nodes = BTreeMap::new();
            let all: Vec<String> = names.iter().map(|s| (*s).to_owned()).collect();
            for name in &all {
                let (dir, store) = crate::convo::store::scratch()?;
                let mgr = Manager::new(name.clone(), Some(store.clone()));
                mgr.register_peers(all.iter().cloned());
                nodes.insert(name.clone(), (mgr, store, dir));
            }
            Ok(Self { nodes })
        }

        fn mgr(&self, name: &str) -> anyhow::Result<&Arc<Manager>> {
            self.nodes.get(name).map(|(m, _, _)| m).ok_or_else(|| anyhow::anyhow!("missing node {name}"))
        }

        fn store(&self, name: &str) -> anyhow::Result<&Store> {
            self.nodes.get(name).map(|(_, s, _)| s).ok_or_else(|| anyhow::anyhow!("missing store {name}"))
        }

        fn settle(&self) -> anyhow::Result<()> {
            for _ in 0..200 {
                let mut moved = false;
                for (from, (mgr, _, _)) in &self.nodes {
                    let rx = mgr.outgoing();
                    while let Ok(msg) = rx.try_recv() {
                        moved = true;
                        if let Some((target, _, _)) = self.nodes.get(&msg.to_name) {
                            target.step(from, &msg.session, &msg.bytes)?;
                        }
                    }
                }
                if !moved {
                    // Drive one beat so newly added learners receive committed entries / conf changes.
                    for (mgr, _, _) in self.nodes.values() {
                        mgr.tick();
                    }
                    let mut after_tick = false;
                    for (from, (mgr, _, _)) in &self.nodes {
                        let rx = mgr.outgoing();
                        while let Ok(msg) = rx.try_recv() {
                            after_tick = true;
                            if let Some((target, _, _)) = self.nodes.get(&msg.to_name) {
                                target.step(from, &msg.session, &msg.bytes)?;
                            }
                        }
                    }
                    if !after_tick {
                        return Ok(());
                    }
                }
            }
            Ok(())
        }
    }

    fn user_event(session: &str, seq: u64, text: &str) -> Row {
        Row::Event(Box::new(EventRow {
            session: session.into(),
            id: format!("ev-{seq}"),
            seq,
            at: 1000 + i64::try_from(seq).unwrap_or(0),
            parent: None,
            by: Some("user:ada".into()),
            kind: "user".into(),
            level: None,
            tool: None,
            status: None,
            text: text.into(),
            body: serde_json::json!({"id": format!("ev-{seq}"), "seq": seq, "at": 1000, "body": {"kind": "user", "text": text}}).to_string(),
        }))
    }

    #[test]
    fn session_started_on_laptop_b_elects_laptop_b_as_raft_leader_and_replicates_across_fleet() -> anyhow::Result<()> {
        let fleet = Fleet::new(&["laptop-a", "laptop-b", "laptop-c", "server"])?;

        // User accesses UI on laptop-a, but starts the session on laptop-b: laptop-b is initial Raft leader!
        let open_row = Row::Session {
            session: "sess-1".into(),
            info: Some(r#"{"id":"sess-1","target":"agent:laptop-b"}"#.into()),
            meta: None,
            state: None,
            changes: None,
            epoch: Some(1),
            open: true,
            at: 1000,
        };
        let st = fleet.mgr("laptop-b")?.open_leader("sess-1", 1, vec![open_row.clone()])?;
        anyhow::ensure!(st.is_leader && st.leader_name.as_deref() == Some("laptop-b"), "laptop-b must be initial Raft leader: {st:?}");

        // Leader writes rows to its own local store (as harness tee does) and proposes through Raft.
        fleet.store("laptop-b")?.put(vec![open_row, user_event("sess-1", 1, "hello from laptop-b")]);
        anyhow::ensure!(fleet.mgr("laptop-b")?.propose("sess-1", 1, vec![user_event("sess-1", 1, "hello from laptop-b")]));

        fleet.settle()?;

        // Every other node (laptop-a, laptop-c, server) knows laptop-b is Raft leader and holds the replicated conversation!
        for peer in ["laptop-a", "laptop-c", "server"] {
            let pst = fleet.mgr(peer)?.status("sess-1").ok_or_else(|| anyhow::anyhow!("{peer} missing sess-1 Raft group"))?;
            anyhow::ensure!(!pst.is_leader && pst.leader_name.as_deref() == Some("laptop-b"), "{peer} must follow laptop-b: {pst:?}");
            fleet.store(peer)?.sync()?;
            let kept = fleet.store(peer)?.kept("sess-1", 10)?.ok_or_else(|| anyhow::anyhow!("{peer} store missing replicated sess-1"))?;
            anyhow::ensure!(kept.events.iter().any(|e| e.text == "hello from laptop-b"), "{peer} missing replicated event: {:?}", kept.events);
        }
        Ok(())
    }

    #[test]
    fn independent_sessions_keep_independent_raft_leaders_on_their_owning_nodes() -> anyhow::Result<()> {
        let fleet = Fleet::new(&["laptop-a", "laptop-b", "server"])?;
        fleet.mgr("laptop-b")?.open_leader("sess-on-b", 1, vec![user_event("sess-on-b", 1, "b work")])?;
        fleet.mgr("laptop-a")?.open_leader("sess-on-a", 1, vec![user_event("sess-on-a", 1, "a work")])?;
        fleet.settle()?;

        anyhow::ensure!(fleet.mgr("laptop-a")?.leader_of("sess-on-b").as_deref() == Some("laptop-b"));
        anyhow::ensure!(fleet.mgr("laptop-b")?.leader_of("sess-on-b").as_deref() == Some("laptop-b"));
        anyhow::ensure!(fleet.mgr("server")?.leader_of("sess-on-b").as_deref() == Some("laptop-b"));

        anyhow::ensure!(fleet.mgr("laptop-a")?.leader_of("sess-on-a").as_deref() == Some("laptop-a"));
        anyhow::ensure!(fleet.mgr("laptop-b")?.leader_of("sess-on-a").as_deref() == Some("laptop-a"));
        anyhow::ensure!(fleet.mgr("server")?.leader_of("sess-on-a").as_deref() == Some("laptop-a"));

        fleet.store("laptop-a")?.sync()?;
        fleet.store("laptop-b")?.sync()?;
        fleet.store("server")?.sync()?;
        let a_kept = fleet.store("laptop-a")?.kept("sess-on-b", 10)?;
        let a_st = fleet.mgr("laptop-a")?.status("sess-on-b");
        anyhow::ensure!(a_kept.as_ref().is_some_and(|k| k.events.len() == 1), "laptop-a sess-on-b kept={a_kept:?} status={a_st:?}");
        let b_kept = fleet.store("laptop-b")?.kept("sess-on-a", 10)?;
        let b_st = fleet.mgr("laptop-b")?.status("sess-on-a");
        anyhow::ensure!(b_kept.as_ref().is_some_and(|k| k.events.len() == 1), "laptop-b sess-on-a kept={b_kept:?} status={b_st:?}");
        anyhow::ensure!(fleet.store("server")?.kept("sess-on-a", 10)?.is_some() && fleet.store("server")?.kept("sess-on-b", 10)?.is_some());
        Ok(())
    }

    #[test]
    fn leadership_transfers_when_session_moves_to_another_runner() -> anyhow::Result<()> {
        let fleet = Fleet::new(&["laptop-a", "laptop-b", "laptop-c"])?;
        fleet.mgr("laptop-b")?.open_leader("sess-move", 1, vec![user_event("sess-move", 1, "before move")])?;
        fleet.settle()?;
        anyhow::ensure!(fleet.mgr("laptop-a")?.leader_of("sess-move").as_deref() == Some("laptop-b"));

        // Move session from laptop-b -> laptop-c: promote laptop-c to voter and transfer Raft leadership!
        anyhow::ensure!(fleet.mgr("laptop-b")?.transfer_leadership("sess-move", "laptop-c"));
        fleet.settle()?;

        let c_status = fleet.mgr("laptop-c")?.status("sess-move").ok_or_else(|| anyhow::anyhow!("laptop-c missing group"))?;
        anyhow::ensure!(c_status.is_leader && c_status.leader_name.as_deref() == Some("laptop-c"), "laptop-c must lead after transfer: {c_status:?}");
        anyhow::ensure!(fleet.mgr("laptop-c")?.propose("sess-move", 2, vec![user_event("sess-move", 2, "after move")]));
        fleet.settle()?;

        fleet.store("laptop-a")?.sync()?;
        let kept = fleet.store("laptop-a")?.kept("sess-move", 10)?.ok_or_else(|| anyhow::anyhow!("laptop-a missing moved session"))?;
        anyhow::ensure!(kept.events.iter().any(|e| e.text == "after move"), "new leader's write must replicate to laptop-a: {:?}", kept.events);
        Ok(())
    }
}
