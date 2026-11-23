//! Per-session/conversation Raft replication (`proto::feature::CONVO_RAFT`): every agent session
//! owns an independent `hyper-raft` consensus group backed by mandatory durable sharded
//! `hyper-log` write-ahead logs (`convo-raft-000.log`, `convo-raft-001.log`, … via
//! `hyper_durable::GroupStore`) whose **initial leader is the node where the session starts and
//! runs** (never necessarily the node whose UI opened it — e.g. opening the UI on `laptop-a:7070`
//! and starting a session on `laptop-b` elects `laptop-b` as that conversation's Raft leader).
//!
//! All conversation writes (`Row::Session`, `Event`, `Reset`, `Closed`, `Record`, `RecordReset`,
//! `Machine`, `Inbox`) proposed on the owning leader are split/chunked to fit the WAL frame budget,
//! persisted to `convo-raft-*.log`, committed through `hyper-raft`, and replicated across every
//! connected node in the fleet (over direct P2P `hyper-quic` links first, falling back to relayed
//! reach links and `LANE_CONVO`).
//!
//! Key durability & lifecycle invariants:
//! - **Per-error `hyper-durable` handling:** transient `Refused`, `Stalled`, `Budget`, and `Marked`
//!   keep rows queued (`Stalled` triggers immediate shard compaction + `resume()`); `Fenced`
//!   reopens the replica cleanly from durable state without losing queued proposals.
//! - **Exact, off-thread, chunked snapshots:** snapshots capture the exact committed frontier at
//!   `applied` (events from `seq == 0`, harness records, inbox rows, session/closed metadata —
//!   never leaking uncommitted future rows from `Store`), are built off the shared owner thread,
//!   and split across `<= sync::wire::PART` (512 KiB) frames on the wire.
//! - **Group lifecycle:** live sessions never evict (full shards auto-open new shards); closed
//!   sessions cleanly retire and release their WAL slot once durably replicated to every member.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::task::{Wake, Waker};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use base64::Engine as _;
use hyper_block::buf::Alignment;
use hyper_block::file::{CachingRequest, DeviceFile};
use hyper_durable::{ENTRY_OVERHEAD, EntryRef, Fatal, GroupStore, LogStore as _, Output, Point, Replica, ReplicaError, Settings, StateMachine, Bytes};
use hyper_log::{Config as LogConfig, Facts, Log, LogError};
use hyper_raft::proto::{ConfChangeSingle, ConfChangeTransition, ConfChangeType, ConfChangeV2, ConfState, EntryType, Message, MessageType};
use hyper_raft::wire::Record as _;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::Digest as _;

use super::rows::{InboxRow, Row};
#[cfg(test)]
use super::rows::RecordRow;
use super::store::Store;
use crate::sync::disk::{BLOCK as WAL_BLOCK, CACHE_BYTES as WAL_CACHE_BYTES, CADENCE_BYTES, CADENCE_ENTRIES, DISK as SHARD_DISK_BYTES, GROUPS as SYNC_GROUPS, UNCOMMITTED_BYTES, UNCOMMITTED_ENTRIES};
use crate::sync::group::{APPEND as APPEND_MAX, Conf, MESSAGE as MESSAGE_MAX, PUMP_MAX as DRIVE_STEPS_MAX, SERVER as SERVER_ID_VAL, save_atomic};
use crate::sync::wire::{Joiner as WireJoiner, PART as WIRE_PART_MAX};

/// Stable Raft member id for the central server node (`1`, matching `sync::group::SERVER`).
pub const SERVER_ID: u64 = SERVER_ID_VAL;
/// Largest single WAL entry payload (`sync::group::MESSAGE`, matching kiln's frame ceiling).
const LARGEST_ENTRY: usize = MESSAGE_MAX;
/// Safe upper bound on one Raft entry's serialized JSON bytes (leaves `ENTRY_OVERHEAD` + one WAL
/// transfer block `sync::disk::BLOCK` for Raft/hyper-log framing inside one segment frame).
const ENTRY_DATA_MAX: usize = LARGEST_ENTRY - ENTRY_OVERHEAD - WAL_BLOCK;
/// Exact maximum serialized byte length of the `WireEntry::Part` JSON envelope excluding `chunk_b64`:
/// `{"chunk_id":18446744073709551615,"part":4294967295,"total":4294967295,"chunk_b64":""}` = 85 bytes.
const CHUNK_ENVELOPE_MAX: usize = 85;
/// Maximum raw JSON bytes per `WireEntry::Part` chunk (exact RFC 4648 base64 3:4 inverse).
const CHUNK_RAW_MAX: usize = ((ENTRY_DATA_MAX - CHUNK_ENVELOPE_MAX) / 4) * 3;
/// A test's bound on its own waits, so a regression fails rather than hangs the suite: no
/// behaviour under test. The manager's own waits are told, never timed.
#[cfg(test)]
const TEST_WAIT: Duration = Duration::from_secs(6);
/// The pause between a test's polls of what it drives: the test's pacing, no behaviour under test.
#[cfg(test)]
pub(crate) const TEST_POLL: Duration = Duration::from_millis(100);

/// RFC 6298 §2.1's retransmission timeout before a path has a round trip measured: "Until a round-trip time (RTT)
/// measurement has been made for a segment sent between the sender and receiver, the sender SHOULD set RTO <- 1
/// second". A repair request to a peer whose path the liveness stream hasn't measured yet counts as lost after it.
const RTO_UNMEASURED: Duration = Duration::from_secs(1);

/// Groups held per `hyper-log` WAL shard (`convo-raft-NNN.log`): `sync::disk::GROUPS^2 = 1024`.
/// At 1024 groups/shard, `hyper-log`'s persist area is ~100 KiB and 10k concurrent live sessions
/// span 10 shards cleanly without ever evicting a live session; closed sessions retire and free slots.
const SHARD_GROUPS: usize = SYNC_GROUPS * SYNC_GROUPS;
/// Fixed log identifier prefix for `convo-raft-NNN.log` (low 32 bits carry the shard number).
const LOG_ID_BASE: u128 = u128::from_be_bytes(*b"kiln-convo-r0000");
/// The fewest voters a fault-tolerant group has: 2f + 1 with f = 1, the smallest ensemble that
/// survives a failure, as ZooKeeper sizes its ensembles. A session's group promotes to its
/// policy's [`Policy::voters`]; a fleet's name charter, and a sealed one, name at least this many.
const MIN_VOTERS: usize = 3;
/// Wire chunk magic prefix for oversized Raft messages (e.g. multi-MiB `MsgSnapshot`) split across
/// `<= sync::wire::PART` frames: `4 magic + 8 id + 4 part + 4 parts` = 20-byte header.
const WIRE_CHUNK_MAGIC: [u8; 4] = *b"KRC1";
const WIRE_CHUNK_HDR: usize = 20;

type ConvoLog = Log<DeviceFile>;
type ConvoStore = GroupStore<DeviceFile>;
type SessionReplica = Replica<ConvoStore, ConvoMachine, Bytes>;

static NEXT_CHUNK_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_COMMIT_TAG: AtomicU64 = AtomicU64::new(1);

/// CP-T3.2. Only the elected member can originate a request in its term. A restarted
/// process campaigns in a new term before issuing another sequence number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct CommitTag {
    member: u64,
    term: u64,
    sequence: u64,
}

impl CommitTag {
    fn new(member: u64, term: u64) -> Self {
        Self { member, term, sequence: NEXT_COMMIT_TAG.fetch_add(1, Ordering::Relaxed) }
    }

    fn context(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(24);
        out.extend_from_slice(&self.member.to_le_bytes());
        out.extend_from_slice(&self.term.to_le_bytes());
        out.extend_from_slice(&self.sequence.to_le_bytes());
        out
    }

    fn from_entry(bytes: &[u8], term: u64) -> Option<Self> {
        let (member, rest) = bytes.split_first_chunk::<8>()?;
        let (issued, rest) = rest.split_first_chunk::<8>()?;
        let sequence: [u8; 8] = rest.try_into().ok()?;
        (u64::from_le_bytes(*issued) == term).then_some(Self {
            member: u64::from_le_bytes(*member),
            term,
            sequence: u64::from_le_bytes(sequence),
        })
    }
}

type ApplyReceipt = (CommitTag, bool, u64);

/// Replication and failover policy for a node's conversation Raft [`Manager`].
///
/// Per `AGENTS.md` ("Opt-in automation"), automatic multi-voter failover is **off by default**
/// (`auto_failover: false`): sessions replicate to connected peers as non-voting learners so
/// a user receipt waits for another durable copy when a replica was present at founding.
/// A real singleton can acknowledge its own durable write. Enable explicitly with `KILN_CONVO_FAILOVER=1` (or [`Manager::open_with_policy`])
/// to promote caught-up peers to a 3-voter quorum with automatic election and ownership takeover.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    pub auto_failover: bool,
    /// The failures f a session's group survives under automatic failover: it promotes 2f + 1
    /// voters ([`Policy::voters`]), as a ZooKeeper ensemble is sized. Configured (`faults` in
    /// `session-policy.json`, or `KILN_CONVO_FAULTS`), 1 when not, and committed with the session
    /// so every replica of it counts the same.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub faults: Option<u32>,
}

impl Policy {
    pub fn from_env() -> Self {
        let auto_failover = std::env::var("KILN_CONVO_FAILOVER")
            .ok()
            .is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"));
        let faults = std::env::var("KILN_CONVO_FAULTS").ok().and_then(|v| v.trim().parse().ok());
        Self { auto_failover, faults }
    }

    /// The voters a session's group promotes to under automatic failover: 2f + 1, at least
    /// [`MIN_VOTERS`].
    pub fn voters(&self) -> usize {
        usize::try_from(self.faults.unwrap_or(1)).unwrap_or(usize::MAX).saturating_mul(2).saturating_add(1).max(MIN_VOTERS)
    }
}

#[derive(Default)]
struct AdmissionState {
    closing: bool,
    queued_bytes: usize,
    incoming: usize,
}

fn reserve_admission(admission: &Mutex<AdmissionState>, bytes: usize) -> Result<(), &'static str> {
    let mut st = admission.lock();
    if st.closing {
        return Err("conversation Raft WAL manager is shutting down");
    }
    if st.queued_bytes.saturating_add(bytes) > crate::memory::room::share() {
        return Err("conversation Raft proposal queue full");
    }
    st.queued_bytes = st.queued_bytes.saturating_add(bytes);
    Ok(())
}

fn release_admission(admission: &Mutex<AdmissionState>, bytes: usize) {
    if bytes > 0 {
        let mut st = admission.lock();
        st.queued_bytes = st.queued_bytes.saturating_sub(bytes);
    }
}

/// Snapshot of process-wide conversation Raft durability & lifecycle counters (also exposed as
/// `kiln_convo_raft_*` Prometheus metrics, on `/healthz/server`, and on `/api/convo/raft`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stats {
    pub dropped_proposals: u64,
    pub corrupt_entries: u64,
    pub reopened_replicas: u64,
    pub retired_groups: u64,
    pub stalled_events: u64,
    pub snapshots_built: u64,
}

/// Current conversation Raft counters backed by [`crate::metrics::convo_raft`].
pub fn stats() -> Stats {
    let m = crate::metrics::convo_raft();
    Stats {
        dropped_proposals: m.dropped_proposals.get(),
        corrupt_entries: m.corrupt_entries.get(),
        reopened_replicas: m.reopened_replicas.get(),
        retired_groups: m.retired_groups.get(),
        stalled_events: m.stalled_events.get(),
        snapshots_built: m.snapshots_built.get(),
    }
}

/// Count of conversation Raft proposals dropped (non-voter refusal, encode error, or unflushable shutdown).
pub fn dropped_proposals() -> u64 {
    crate::metrics::convo_raft().dropped_proposals.get()
}

/// Count of committed conversation Raft WAL entries that failed to decode or reassemble.
pub fn corrupt_entries() -> u64 {
    crate::metrics::convo_raft().corrupt_entries.get()
}

/// Count of fenced conversation Raft replicas reopened from durable state.
pub fn reopened_replicas() -> u64 {
    crate::metrics::convo_raft().reopened_replicas.get()
}

/// Count of closed conversation Raft groups retired and removed from WAL once replicated.
pub fn retired_groups() -> u64 {
    crate::metrics::convo_raft().retired_groups.get()
}

fn wal_lock(path: &Path) -> anyhow::Result<std::fs::File> {
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path)?;
    lock.try_lock().with_context(|| format!("conversation WAL lock is held at {}", path.display()))?;
    Ok(lock)
}

/// Move a closed conversation Raft WAL directory (`convo-raft`) cleanly from `from` to `to`
/// during a blue/green upgrade handover or rollback. Never silently skips when `to` already
/// exists: sets the stale destination aside with a warning and propagates any I/O error.
pub fn move_wal_dir(from: &Path, to: &Path) -> anyhow::Result<()> {
    move_wal_dir_checked(from, to, false)
}

/// [`move_wal_dir`], failing explicitly if `require_source` is true and `from` does not exist
/// (used on rollback so a missing/deleted WAL is never silently replaced with an empty log).
pub fn move_wal_dir_checked(from: &Path, to: &Path, require_source: bool) -> anyhow::Result<()> {
    if from == to { anyhow::ensure!(!require_source || from.is_dir(), "required WAL directory is absent"); return Ok(()); }
    #[derive(Serialize, Deserialize)]
    struct Move {
        from: PathBuf,
        to: PathBuf,
        generation: String,
        aside: PathBuf,
        complete: bool,
    }
    let source_dir = from.parent().context("WAL source has no parent")?;
    let target_dir = to.parent().context("WAL destination has no parent")?;
    std::fs::create_dir_all(target_dir)?;
    anyhow::ensure!(source_dir.is_dir() || !require_source, "missing WAL source directory {}", source_dir.display());
    let mut lock_paths = BTreeSet::new();
    for path in [from, to] {
        if let Some(dir) = path.parent().filter(|parent| parent.is_dir()) {
            lock_paths.insert(std::fs::canonicalize(dir)?.join(path.file_name().context("invalid WAL path")?).with_extension("move-lock"));
        }
    }
    let _locks = lock_paths.into_iter().map(|path| wal_lock(&path)).collect::<anyhow::Result<Vec<_>>>()?;
    let _owners = [from, to].into_iter().filter(|dir| dir.is_dir()).map(|dir| wal_lock(&dir.join(".wal-owner.lock"))).collect::<anyhow::Result<Vec<_>>>()?;
    let receipt = to.with_extension("incoming.json");
    let old: Option<Move> = match std::fs::read(&receipt) {
        Ok(bytes) => Some(serde_json::from_slice(&bytes).context("invalid WAL move journal")?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let write = |record: &Move| save_atomic(&receipt, &serde_json::to_vec(record)?);
    let marker = |dir: &Path| dir.join(".wal-generation");
    if !from.try_exists()? {
        if let Some(mut record) = old {
            anyhow::ensure!(record.from == from && record.to == to && std::fs::read_to_string(marker(to))? == record.generation, "WAL move journal disagrees with destination generation");
            if !record.complete {
                crate::blobs::chunks::sync_dir(source_dir)?;
                crate::blobs::chunks::sync_dir(target_dir)?;
                record.complete = true;
                write(&record)?;
            }
            return Ok(());
        }
        anyhow::ensure!(!require_source, "required conversation WAL source {} is missing", from.display());
        return Ok(());
    }
    let generation = match std::fs::read_to_string(marker(from)) {
        Ok(id) => id,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let id = crate::util::new_id();
            save_atomic(&marker(from), id.as_bytes())?;
            id
        }
        Err(e) => return Err(e.into()),
    };
    let mut record = if let Some(old) = old.filter(|r| !r.complete) {
        anyhow::ensure!(old.from == from && old.to == to && old.generation == generation, "an unfinished WAL move names a different source generation");
        old
    } else {
        let record = Move { from: from.to_owned(), to: to.to_owned(), generation, aside: to.with_extension(format!("set-aside-{}", crate::util::new_id())), complete: false };
        write(&record)?;
        record
    };
    if to.try_exists()? {
        anyhow::ensure!(!record.aside.try_exists()?, "WAL destination changed while a move was incomplete");
        std::fs::rename(to, &record.aside)?;
        crate::blobs::chunks::sync_dir(target_dir)?;
    }
    std::fs::rename(from, to).with_context(|| format!("moving conversation WAL {} to {}", from.display(), to.display()))?;
    crate::blobs::chunks::sync_dir(target_dir)?;
    crate::blobs::chunks::sync_dir(source_dir)?;
    record.complete = true;
    write(&record)?;
    Ok(())
}

/// Persistent unique Raft node name for a `kiln server` installation (`<wal_dir>/node-id`).
/// Gives every full server its own deterministic member ID (`>= 2`) so starting multiple full
/// servers on different nodes never collides on member ID 1.
pub fn server_node_name(wal_dir: &Path) -> anyhow::Result<String> {
    std::fs::create_dir_all(wal_dir).with_context(|| format!("creating {}", wal_dir.display()))?;
    let path = wal_dir.join("node-id");
    use std::io::Write as _;
    let read = || -> anyhow::Result<String> {
        let raw = std::fs::read_to_string(&path)?;
        let name = raw.trim();
        anyhow::ensure!(!name.is_empty(), "empty conversation Raft node identity");
        Ok(name.to_owned())
    };
    match read() {
        Ok(name) => return Ok(name),
        Err(e) if e.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) => {}
        Err(e) => return Err(e),
    }
    // Install once across concurrent starts. Preserve all UUID bits across hosts.
    let name = format!("server-{}", crate::util::new_id());
    let mut tmp = tempfile::NamedTempFile::new_in(wal_dir)?;
    tmp.write_all(name.as_bytes())?;
    tmp.as_file().sync_all()?;
    match tmp.persist_noclobber(&path) {
        Ok(_) => { crate::blobs::chunks::sync_dir(wal_dir)?; Ok(name) }
        Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => read(),
        Err(e) => Err(e.error.into()),
    }
}

/// Deterministic Raft node ID for a kiln node name (`"server"` -> `1`, every runner -> `>= 2`),
/// matching `crate::store::group::member_of` so every node in the fleet computes the exact same id.
pub fn node_id(name: &str) -> u64 {
    if name == "server" {
        return SERVER_ID;
    }
    crate::store::group::member_of(name)
}

/// Deterministic non-zero 128-bit `hyper-log` group id for `session`.
fn group_id(session: &str) -> u128 {
    let digest = sha2::Sha256::digest(session.as_bytes());
    let bytes = digest.as_slice().first_chunk::<16>().copied().unwrap_or([0; 16]);
    u128::from_be_bytes(bytes).max(1)
}

fn shard_log_id(shard: u32) -> u128 {
    LOG_ID_BASE ^ u128::from(shard)
}

fn shard_path(dir: &Path, shard: u32) -> PathBuf {
    dir.join(format!("convo-raft-{shard:03}.log"))
}

/// CP-T3.2. The committed native package a specific successor may activate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveIntent {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    leadership_only: bool,
    #[serde(default)]
    cancel: bool,
    #[serde(default)]
    ready: bool,
    epoch: u64,
    target: String,
    digest: String,
}

/// A durable vote for exactly one founding operation under this session id. The immutable
/// electorate includes disconnected enrolled replicas; a majority of one common roster intersects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Genesis {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    charter: Option<[u8; 32]>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    witnesses: Vec<u64>,
    index: u64,
    digest: [u8; 32],
    members: Vec<u64>,
}

impl Genesis {
    fn majority(&self, witnesses: &[u64]) -> bool {
        witnesses.len() > self.members.len() / 2 && witnesses.windows(2).all(|w| w.first() < w.last())
            && witnesses.iter().all(|id| self.members.binary_search(id).is_ok())
    }
}

/// One entry committed to a conversation's Raft log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConvoEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    process_loss: Option<Box<super::custody::Certificate>>,
    /// Authenticated process incarnation selected for this new execution epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner_run: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    reaffirm: bool,
    /// A seal carries its full electorate for a replica that received the new charter
    /// after the provisional opening bytes were committed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    seal_voters: Vec<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    genesis_charter: Option<[u8; 32]>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    genesis_members: Vec<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    seal: Option<[u8; 32]>,
    // False creates a provisional identity. True publishes it after the founder has
    // verified remote durability; every replica can then answer start-status requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    opening: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    native_move: Option<MoveIntent>,
    pub session: String,
    pub leader: String,
    #[serde(default)]
    pub epoch: u64,
    pub rows: Vec<Row>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<Policy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_from: Option<u64>,
}

/// The enrollment authority signs one immutable witness set. The small local pin and
/// freeze move with this node's WAL. Enrollment owns the signed, potentially large history.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct NamePin {
    /// A migrated registrar may never forget that pre-existing holders must be consulted.
    #[serde(default)]
    needs_inventory: bool,
    registrar: Option<String>,
    frozen: bool,
    charter: Option<[u8; 32]>,
    /// Provisional slots present at the original registrar's atomic cutoff. The
    /// authenticated selected node is known even before its last WAL part arrives.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pending: BTreeMap<String, FrozenOrigin>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FrozenOrigin {
    selected: String,
    digest: Option<[u8;32]>,
}

struct NameCharter {
    digest: [u8; 32],
    voters: Vec<u64>,
    reserved: Arc<BTreeSet<String>>,
}

#[derive(Default)]
struct NameAdmission {
    required: bool,
    pin: NamePin,
    active: Option<Arc<NameCharter>>,
    historical: Option<Arc<BTreeSet<String>>>,
}

impl NameAdmission {
    fn registrar_can_grant(&self, session: &str) -> bool {
        !self.pin.needs_inventory || self.historical.as_ref().is_some_and(|ids| !ids.contains(session))
    }

    fn certified(&self, session: &str, genesis: &Genesis, published: bool) -> bool {
        if !self.required { return true; }
        if let Some(charter) = &self.active {
            if genesis.charter == Some(charter.digest) {
                return genesis.members == charter.voters && !charter.reserved.contains(session);
            }
            // An original registrar witness can finish an operation across the cutoff
            // only for an id captured in its durable historical reservation set.
            if !charter.reserved.contains(session) { return false; }
        } else if self.pin.charter.is_some() || (self.pin.frozen && !published && !self.pin.pending.contains_key(session)) {
            return false;
        }
        genesis.charter.is_none() && self.pin.registrar.as_ref().is_some_and(|r| genesis.members.as_slice() == [node_id(r)])
    }
}

enum NameChange {
    Registrar(String),
    Freeze,
    Inventory,
    Historical { incomplete: bool, reserved: Arc<BTreeSet<String>> },
    Require([u8;32]),
    Install(Arc<NameCharter>),
}

/// Wire representation stored inside a single WAL/Raft entry. Normal entries encode directly as
/// `ConvoEntry` JSON (`Whole`); any single row whose encoded `ConvoEntry` exceeds `ENTRY_DATA_MAX`
/// is split into ordered `Part` chunks so arbitrarily large tool outputs never drop.
#[derive(Serialize, Deserialize)]
struct WirePart<'a> {
    chunk_id: u64,
    part: u32,
    total: u32,
    #[serde(borrow)]
    chunk_b64: std::borrow::Cow<'a, str>,
}

mod b64_bytes {
    use base64::Engine as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(&base64::display::Base64Display::new(v, &base64::engine::general_purpose::STANDARD))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<std::sync::Arc<Vec<u8>>, D::Error> {
        base64::engine::general_purpose::STANDARD.decode(String::deserialize(d)?).map(std::sync::Arc::new).map_err(serde::de::Error::custom)
    }
}

/// Partially reassembled oversized `ConvoEntry` awaiting remaining `WireEntry::Part` entries.
/// Tracks the Raft `term` of part 0 so any chunk stranded by a leader crash or step-down is
/// deterministically discarded on every replica the instant a new term commits its no-op entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct InFlightPart {
    #[serde(default)]
    term: u64,
    next_part: u32,
    total: u32,
    // A pinned checkpoint shares its immutable prefix with the live decoder.
    #[serde(with = "b64_bytes")]
    bytes: Arc<Vec<u8>>,
}

/// Committed session metadata as of `ConvoMachine::applied` (mirrors SQL `SESSION_UPSERT` COALESCE
/// semantics so snapshots never read uncommitted future metadata from `Store`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct AppliedSession {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    info: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    meta: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    changes: Option<String>,
    #[serde(default)]
    updated_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    closed_at: Option<i64>,
}

/// Snapshot payload transferred when a late-joining peer is behind the compacted Raft log.
/// Carries the FULL committed state at `at` (session metadata, closed marker, every event from
/// `seq == 0`, every harness record source/line, and every inbox family row) so the peer can
/// resume directly without missing history or applying out-of-order rows.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct ConvoImage {
    #[serde(default,skip_serializing_if="Option::is_none")]
    configuration: Option<Conf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    genesis: Option<Genesis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    founding_index: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    native_move: Option<MoveIntent>,
    /// CP-T3.2. The last hand-over given up, so a member that catches up later learns it too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    abandoned_move: Option<MoveIntent>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    in_flight_parts: BTreeMap<u64, InFlightPart>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner_run: Option<u64>,
    #[serde(default)]
    owner_term: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    policy: Option<Policy>,
    session: String,
    leader: String,
    epoch: u64,
    rows: Vec<Row>,
}

/// Persisted shard + WAL group id mapping kept in `<dir>/sessions.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct SessionSlot {
    shard: u32,
    gid: u128,
}

/// A terminal image's durable consensus identity. It can reconstitute a compacted
/// WAL slot so a member enrolled after closure can still catch up.
#[derive(Clone, Serialize, Deserialize)]
struct TerminalGroup {
    session: String,
    index: u64,
    term: u64,
    hard: Vec<u8>,
    members: Vec<u64>,
}

/// Durable per-session state machine checkpoint kept beside `convo-raft-*.log` so restarts recover
/// exact `(durable_point, configuration, epoch, leader_name, applied_session, closed_at_index)`
/// both before and after WAL compaction. Holds lightweight metadata only — conversation rows live
/// durably in `Store` (fsynced before advancing `durable`).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SavedConvo {
    #[serde(default,skip_serializing_if="Option::is_none")]
    electorate_origin: Option<Arc<(u64,Conf)>>,
    /// Validated on this member under its original, authenticated namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    verified_genesis: Option<[u8;32]>,
    #[serde(default)]
    name_vote: Option<[u8;32]>,
    #[serde(default)]
    legacy_authorized: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    genesis: Option<Genesis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    founding_index: Option<u64>,
    #[serde(default)]
    founding_barrier: FoundingBarrier,
    #[serde(default)]
    projected: Option<(u64, u64)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    native_move: Option<MoveIntent>,
    /// CP-T3.2. The last hand-over given up, so a member that catches up later learns it too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    abandoned_move: Option<MoveIntent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner_run: Option<u64>,
    #[serde(default)]
    owner_term: u64,
    #[serde(default)]
    recovery_image: Option<SavedImage>,
    #[serde(default)]
    chunked_through: u64,
    #[serde(default)]
    founded: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    policy: Option<Policy>,
    session: String,
    leader_name: String,
    epoch: u64,
    index: u64,
    term: u64,
    conf: Conf,
    #[serde(default)]
    applied_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    closed_at_index: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    applied_session: Option<AppliedSession>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    in_flight_parts: BTreeMap<u64, InFlightPart>,
}

struct SnapshotJob {
    message: Message,
    session: String,
    to_name: String,
    path: PathBuf,
    id: u64,
    total: u32,
}

impl SnapshotJob {
    fn stage(self) -> anyhow::Result<SnapshotSend> {
        let Self { message, session, to_name, path, id, total } = self;
        let index = message.snapshot.as_deref().and_then(|s| s.metadata.as_ref()).context("snapshot has no index")?.index;
        let term = message.term;
        let to_id = message.to;
        let bytes = message.encode_to_vec();
        drop(message);
        let source = SnapshotSource::new(bytes, &path)?;
        Ok(SnapshotSend { session, to_name, to_id, index, term, id, part: 0, total, source })
    }
}

struct SnapshotSend {
    session: String,
    to_id: u64,
    to_name: String,
    source: SnapshotSource,
    index: u64,
    term: u64,
    id: u64,
    part: u32,
    total: u32,
}

/// Keep at most one frame per waiting peer in memory. Larger Raft envelopes are
/// spooled beside the WAL and their anonymous file is reclaimed on cancel or crash.
enum SnapshotSource {
    Frame(Vec<u8>),
    Spool { file: std::fs::File, len: usize, checksums: Vec<u32> },
}

impl SnapshotSource {
    fn new(bytes: Vec<u8>, state_path: &Path) -> anyhow::Result<Self> {
        use std::io::Write as _;
        if bytes.len() <= WIRE_PART_MAX { return Ok(Self::Frame(bytes)); }
        let dir = state_path.parent().context("conversation state has no directory")?;
        let len = bytes.len();
        anyhow::ensure!(crate::observe::disk_free(dir).is_some_and(|free| free >= u64::try_from(len).unwrap_or(u64::MAX)), "insufficient disk space for outgoing snapshot");
        let mut file = tempfile::tempfile_in(dir)?;
        // CP-T3.2. Keep the same per-spilled-buffer CRC as harness::spill. The
        // authenticated wire carries the original bytes and needs no new format.
        let frames = bytes.chunks(WIRE_PART_MAX - WIRE_CHUNK_HDR);
        let mut checksums = Vec::new();
        checksums.try_reserve_exact(frames.len())?;
        for frame in frames { checksums.push(crc32fast::hash(frame)); file.write_all(frame)?; }
        Ok(Self::Spool { file, len, checksums })
    }
}

impl SnapshotSend {
    /// Only materialize the next disk frame after its exact queue credit is available.
    fn has_credit(&self, queue: &crate::memory::queue::Sender<Outgoing>) -> bool {
        let held = queue.held();
        if held == 0 { return true; }
        let bytes = match &self.source {
            SnapshotSource::Frame(bytes) => bytes.capacity(),
            SnapshotSource::Spool { len, .. } => {
                let stride = WIRE_PART_MAX - WIRE_CHUNK_HDR;
                let start = usize::try_from(self.part).unwrap_or(usize::MAX).saturating_mul(stride);
                len.saturating_sub(start).min(stride).saturating_add(WIRE_CHUNK_HDR)
            }
        };
        let weight = size_of::<Outgoing>().saturating_add(self.session.len()).saturating_add(self.to_name.len()).saturating_add(bytes);
        held.saturating_add(weight) <= queue.bound()
    }

    fn frame(&mut self) -> anyhow::Result<Outgoing> {
        use std::io::{Read as _, Seek as _};
        let bytes = match &mut self.source {
            SnapshotSource::Frame(bytes) => std::mem::take(bytes),
            SnapshotSource::Spool { file, len, checksums } => {
                let size = WIRE_PART_MAX - WIRE_CHUNK_HDR;
                let start = usize::try_from(self.part)?.checked_mul(size).context("snapshot offset overflow")?;
                let take = len.checked_sub(start).context("invalid snapshot offset")?.min(size);
                let mut out = Vec::with_capacity(WIRE_CHUNK_HDR + take);
                out.extend_from_slice(&WIRE_CHUNK_MAGIC);
                out.extend_from_slice(&self.id.to_be_bytes());
                out.extend_from_slice(&self.part.to_be_bytes());
                out.extend_from_slice(&self.total.to_be_bytes());
                out.resize(WIRE_CHUNK_HDR + take, 0);
                file.seek(std::io::SeekFrom::Start(u64::try_from(start)?))?;
                let payload = out.get_mut(WIRE_CHUNK_HDR..).context("snapshot frame has no payload")?;
                file.read_exact(payload)?;
                anyhow::ensure!(checksums.get(usize::try_from(self.part)?) == Some(&crc32fast::hash(payload)), "outgoing snapshot spool checksum mismatch");
                out
            }
        };
        Ok(Outgoing { session: self.session.clone(), to_id: self.to_id, to_name: self.to_name.clone(), bytes, is_snapshot: true, snapshot_last: self.part + 1 == self.total })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SavedImage { index: u64, term: u64, digest: String, #[serde(default)] content_named: bool }

impl SavedImage {
    fn path(&self, state: &Path) -> PathBuf {
        if self.content_named { state.with_extension(format!("image-{}-{}-{}", self.index, self.term, self.digest)) }
        else { state.with_extension(format!("image-{}-{}", self.index, self.term)) }
    }
}

/// An outgoing Raft message produced by a session's Raft group for a specific peer node.
#[derive(Debug, Clone)]
pub struct Outgoing {
    pub session: String,
    pub to_id: u64,
    pub to_name: String,
    pub bytes: Vec<u8>,
    /// True for `MsgSnapshot` frames (including split `KRC1` snapshot chunks) so transport reports
    /// real enqueue/delivery status back to Raft via [`Manager::report_snapshot`].
    pub is_snapshot: bool,
    /// True on the final frame of a snapshot bundle (single-frame or last `KRC1` chunk) so
    /// transport reports `report_snapshot(..., true)` only once every frame was actually sent.
    pub snapshot_last: bool,
}

impl crate::memory::queue::Weigh for Outgoing {
    fn weight(&self) -> usize {
        size_of::<Self>().saturating_add(self.session.capacity()).saturating_add(self.to_name.capacity()).saturating_add(self.bytes.capacity())
    }
}

/// Ownership transition emitted by [`Manager::failover_events`] when a session's Raft group changes
/// leadership (e.g. automatic election after leader crash, or returning old leader stepping down).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailoverEvent {
    /// This node won a Raft election for a live session previously owned by `previous_leader`,
    /// claimed `new_epoch` durably in Raft, and must now resume harness + inbox execution locally.
    Claimed {
        session: String,
        new_epoch: u64,
        previous_leader: String,
        term: u64,
    },
    /// This node lost Raft leadership for `session` to `new_leader` at `new_epoch` and must fence
    /// its stale local harness + inbox so it cannot act under the superseded ownership epoch.
    LostLeadership {
        session: String,
        new_epoch: u64,
        new_leader: String,
        term: u64,
    },
}

/// Public view of one session's Raft group (exposed on `/healthz/server` and `/api/convo/raft`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupStatus {
    /// A healthy provider remains on its committed execution node during disk handover.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub execution_held: bool,
    /// This local unpublished origin cannot acquire enough votes for the requested name.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub name_conflict: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflicting_start_request: Option<String>,
    /// Durable session identity is known, but its WAL or projection cannot yet authorize work.
    #[serde(default)]
    pub recovering: bool,
    /// Present only for the committed destination and its exact new custody epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_move_digest: Option<String>,
    #[serde(default)]
    pub native_move_ready: bool,
    /// CP-T3.2. The hand-over its old leader gave up at this epoch: whoever waits on it is told.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abandoned_move: Option<AbandonedMove>,
    #[serde(default)]
    pub founded: bool,
    pub session: String,
    pub node: String,
    pub node_id: u64,
    pub is_leader: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    pub leader_id: u64,
    pub leader_name: Option<String>,
    pub term: u64,
    pub applied: u64,
    pub epoch: u64,
    pub voters: Vec<u64>,
    pub learners: Vec<u64>,
}

/// CP-T3.2. A hand-over its old leader gave up: the epoch it was asked at, its target and the
/// staged package's digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbandonedMove {
    pub epoch: u64,
    pub target: String,
    pub digest: String,
}

/// Read every inbox family row for `session` from `store` — BOTH live rows (`value: Some(..)`)
/// and delete tombstones (`value: None`), ordered oldest-first by `rev` — so the session's leader
/// replicates both accepted-message custody AND delivery cleanup through Raft for zero-duplication
/// failover (delivered pending rows are deleted on followers instead of re-delivering).
pub fn session_inbox_rows(store: &Store, session: &str) -> anyhow::Result<Vec<Row>> {
    let entries = store.inbox_session_all(session)?;
    let mut rows = Vec::with_capacity(entries.len());
    for (family, key, value) in entries {
        rows.push(Row::Inbox(Box::new(InboxRow {
            session: session.to_owned(),
            family,
            key,
            value,
        })));
    }
    Ok(rows)
}

/// Exact committed frontier handed to an off-thread snapshot worker so snapshots are built off the
/// shared `"kiln-convo-raft"` owner thread and reflect the EXACT state at `at` (never leaking
/// uncommitted rows written ahead to `Store`).
struct SnapshotSpec {
    genesis: Option<Genesis>,
    founding_index: Option<u64>,
    native_move: Option<MoveIntent>,
    abandoned_move: Option<MoveIntent>,
    in_flight_parts: BTreeMap<u64, InFlightPart>,
    owner_run: Option<u64>,
    owner_term: u64,
    policy: Option<Policy>,
    session: String,
    store: Store,
    leader_name: String,
    epoch: u64,
    at: Point,
    conf: ConfState,
    applied_session: Option<AppliedSession>,
}

fn build_snapshot_bytes(spec: SnapshotSpec, committed: Receiver<anyhow::Result<Vec<Row>>>) -> Result<Vec<u8>, String> {
    let mut rows = committed.recv().map_err(|_| "pinned conversation snapshot reader stopped")?.map_err(|e| format!("reading committed snapshot: {e:#}"))?;
    let original = rows.len();
    let additional = 1 + usize::from(spec.applied_session.is_some()) + usize::from(spec.applied_session.as_ref().is_some_and(|s| s.closed_at.is_some())) + usize::from(!spec.leader_name.is_empty());
    rows.try_reserve_exact(additional).map_err(|e| e.to_string())?;
    let snap_at = spec.applied_session.as_ref().map_or(0, |s| s.updated_at);
    // Clear any stale pre-snapshot events on the installing replica before replaying the exact
    // committed timeline at `spec.at`.
    rows.push(Row::Reset { session: spec.session.clone(), from: None, at: snap_at });
    if let Some(sess) = spec.applied_session {
        rows.push(Row::Session {
            session: spec.session.clone(),
            info: sess.info,
            meta: sess.meta,
            state: sess.state,
            changes: sess.changes,
            epoch: Some(spec.epoch),
            open: sess.closed_at.is_none(),
            at: sess.updated_at,
        });
        if let Some(at) = sess.closed_at {
            rows.push(Row::Closed { session: spec.session.clone(), at });
        }
    }
    let header = rows.len() - original;
    rows.rotate_right(header);
    if !spec.leader_name.is_empty() {
        rows.push(Row::Machine { session: spec.session.clone(), machine: spec.leader_name.clone() });
    }
    let img = ConvoImage { configuration:Some(Conf::from(&spec.conf)), genesis: spec.genesis, founding_index: spec.founding_index, native_move: spec.native_move, abandoned_move: spec.abandoned_move, in_flight_parts: spec.in_flight_parts, owner_run:spec.owner_run, owner_term: spec.owner_term, policy: spec.policy, session: spec.session, leader: spec.leader_name, epoch: spec.epoch, rows };
    // Large images run through an off-owner counting writer once to reserve their exact
    // wire size. Escaped event bodies and partial-command base64 cannot double the allocation.
    let estimate = img.rows.iter().map(Row::bytes).fold(0usize, usize::saturating_add);
    let size = if estimate > WIRE_PART_MAX || !img.in_flight_parts.is_empty() { crate::memory::queue::json_bytes(&img) } else { estimate };
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(size).map_err(|e| e.to_string())?;
    serde_json::to_writer(&mut bytes, &img).map_err(|e| format!("conversation Raft snapshot failed to encode: {e}"))?;
    Ok(bytes)
}

/// Whether `row` is a `Row::Session` whose `info.target` explicitly names a different runner than
/// the transport-authenticated `authenticated_leader` (enforced both on group creation AND on apply
/// so an entry-less heartbeat can never bypass custody isolation).
fn info_violates_sender_custody(info: Option<&str>, authenticated_leader: &str) -> bool {
    if authenticated_leader.is_empty() { return true; }
    #[derive(Deserialize)]
    struct Target<'a> { #[serde(borrow)] target: std::borrow::Cow<'a, str> }
    let Some(target) = info.and_then(|raw| serde_json::from_str::<Target<'_>>(raw).ok()) else { return false; };
    let node = target.target.strip_prefix("agent:").unwrap_or(&target.target).trim();
    !node.is_empty() && node != "local" && node != authenticated_leader
}

fn row_violates_sender_custody(row: &Row, authenticated_leader: &str) -> bool {
    let info = if let Row::Session { info, .. } = row { info.as_deref() } else { None };
    info_violates_sender_custody(info, authenticated_leader)
}

fn valid_digest(digest: &str) -> bool {
    digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn valid_row(row: &Row, session: &str, leader: &str, epoch: u64) -> bool {
    row.session() == session && !row_violates_sender_custody(row, leader) &&
        !matches!(row, Row::Inbox(inbox) if (crate::harness::inbox::FAMILIES.whose)(&inbox.family, &inbox.key) != Some(session.as_bytes())) &&
        !matches!(row, Row::Session { epoch: Some(sent), .. } if *sent != epoch)
}


/// Per-session state machine backed by this node's local Turso conversation [`Store`] and a tiny
/// metadata checkpoint file beside `convo-raft-*.log`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
enum FoundingBarrier {
    #[default]
    Local,
    RemoteCopy,
    VotingFleet,
}

struct ConvoMachine {
    electorate_origin: Arc<(u64,Conf)>,
    local_id: u64,
    verified_genesis: Option<[u8;32]>,
    name_vote: Option<[u8;32]>,
    legacy_authorized: bool,
    names: Arc<parking_lot::RwLock<NameAdmission>>,
    genesis: Option<Genesis>,
    founding_index: Option<u64>,
    // Local founding promise persisted before the WAL slot is published. A restart without
    // peer discovery cannot turn a promised remote receipt into a local-only receipt.
    founding_barrier: FoundingBarrier,
    projected: Option<(u64, u64)>,
    native_move: Option<MoveIntent>,
    /// CP-T3.2. The last hand-over given up, so a member that catches up later learns it too.
    abandoned_move: Option<MoveIntent>,
    owner_run: Option<u64>,
    owner_term: u64,
    recovery_image: Option<SavedImage>,
    recovery_rows: Option<Vec<Row>>,
    chunked_through: u64,
    founded: bool,
    policy: Option<Policy>,
    session: String,
    /// Authenticated leader name for this Raft group (bound to the Raft group's transport-verified
    /// leader rather than an unverified string inside an entry).
    leader_name: String,
    epoch: u64,
    /// Raft applied index at which `Row::Closed` was applied (cleared if reopened via `Row::Session { open: true }`).
    closed_at_index: Option<u64>,
    store: Option<Store>,
    applied: Point,
    durable: Point,
    configuration: ConfState,
    state_file: PathBuf,
    /// Actual cumulative bytes applied (real size signal for `hyper-durable` compaction ratio).
    applied_bytes: u64,
    applied_session: Option<AppliedSession>,
    /// Multi-part chunks currently being reassembled for single rows exceeding `ENTRY_DATA_MAX`.
    in_flight_parts: BTreeMap<u64, InFlightPart>,
    /// Pre-built snapshot image produced off the shared owner thread, swapped in O(1) inside `image()`.
    prepared_image: Option<(Point, ConfState, Vec<u8>)>,
    #[cfg(test)]
    fail_next_persist: Arc<AtomicBool>,
}

impl ConvoMachine {
    fn published(&self) -> bool {
        self.founded && self.founding_index.is_none() && self.genesis.as_ref().map_or(self.legacy_authorized, |g|
            self.verified_genesis == Some(g.digest) || (g.majority(&g.witnesses) && self.names.read().certified(&self.session, g, true)))
    }

    fn may_publish(&self, g: &Genesis) -> bool { self.names.read().certified(&self.session, g, self.founding_index.is_none()) }

    // This durable, local vote never rides in images. A joined or repaired member can
    // acknowledge the winning log without voting for two different origins of the same id.
    fn record_name_vote(&mut self) -> Result<(), Fatal> {
        // An unsealed original-registrar candidate absent from its signed cutoff never
        // received that registrar's vote. Keep the exact opening bytes and selected node.
        // The new immutable electorate can now safely vote for that same digest.
        if self.founding_index.is_some() && let Some(g) = self.genesis.as_mut().filter(|g| g.charter.is_none()) {
            let n = self.names.read();
            if let Some(next) = n.active.as_ref() && !next.reserved.contains(&self.session)
                && n.pin.registrar.as_ref().is_some_and(|r| g.members.as_slice() == [node_id(r)]) {
                g.charter = Some(next.digest);
                g.members.clone_from(&next.voters);
                drop(n);
                self.save_checkpoint()?;
            }
        }
        let Some(g) = self.genesis.as_ref() else { return Ok(()) };
        if self.founding_index.is_none() {
            // Once sealed, this exact session can keep running if the fleet directory
            // or historical-name cache becomes unavailable. New sessions still need it.
            if self.verified_genesis != Some(g.digest) && g.majority(&g.witnesses) {
                let n = self.names.read();
                if n.required && n.certified(&self.session, g, true) {
                    self.verified_genesis = Some(g.digest);
                    drop(n);
                    return self.save_checkpoint();
                }
            }
            return Ok(());
        }
        if self.name_vote.is_some() || g.members.binary_search(&self.local_id).is_err() { return Ok(()); }
        let n = self.names.read();
        if !n.certified(&self.session, g, false) { return Ok(()); }
        if n.required && g.charter.is_none() && !n.pin.frozen && !n.registrar_can_grant(&self.session) { return Ok(()); }
        if n.required && n.pin.frozen && g.charter.is_none() {
            let allowed = n.pin.pending.get(&self.session).is_some_and(|slot|
                slot.selected == self.leader_name && slot.digest.is_none_or(|old| old == g.digest));
            if !allowed { return Ok(()); }
        }
        self.name_vote = Some(g.digest);
        drop(n);
        self.save_checkpoint()
    }
    fn open(session: String, names: Arc<parking_lot::RwLock<NameAdmission>>, local_id: u64, opening:Option<(String,ConfState)>, store: Option<Store>, state_file: PathBuf) -> anyhow::Result<Self> {
        if state_file.exists() {
            let raw = std::fs::read(&state_file).with_context(|| format!("reading {}", state_file.display()))?;
            let saved: SavedConvo = serde_json::from_slice(&raw).with_context(|| format!("decoding {}", state_file.display()))?;
            let at = Point { index: saved.index, term: saved.term };
            anyhow::ensure!(saved.session == session, "conversation checkpoint names a different session");
            anyhow::ensure!(saved.recovery_image.as_ref().is_none_or(|image| valid_digest(&image.digest)), "invalid conversation recovery-image digest");
            store.as_ref().context("conversation store unavailable")?.begin_raft_projection(&session, at.index)?;
            return Ok(Self {
                electorate_origin:saved.electorate_origin.unwrap_or_else(||Arc::new((saved.index,saved.conf.clone()))),
                local_id,
                name_vote: saved.name_vote, verified_genesis: saved.verified_genesis,
                legacy_authorized: saved.legacy_authorized.unwrap_or(saved.genesis.is_none() && saved.founded.unwrap_or(saved.index != 0)),
                names,
                genesis: saved.genesis,
                founding_barrier: saved.founding_barrier,
                founding_index: saved.founding_index,
                projected: saved.projected,
                native_move: saved.native_move,
                abandoned_move: saved.abandoned_move,
                owner_run:saved.owner_run, owner_term: saved.owner_term,
                recovery_image: saved.recovery_image,
                recovery_rows: None,
                chunked_through: saved.chunked_through,
                founded: saved.founded.unwrap_or(saved.applied_bytes != 0 && saved.in_flight_parts.is_empty()),
                policy: saved.policy,
                session,
                leader_name: saved.leader_name,
                epoch: saved.epoch,
                closed_at_index: saved.closed_at_index,
                store,
                applied: at,
                durable: at,
                configuration: (&saved.conf).into(),
                state_file,
                applied_bytes: saved.applied_bytes,
                applied_session: saved.applied_session,

                in_flight_parts: saved.in_flight_parts,
                prepared_image: None,
                #[cfg(test)]
                fail_next_persist: Arc::new(AtomicBool::new(false)),
            });
        }
        let (default_leader,initial_conf)=opening.with_context(||format!("saved conversation {session} is missing its ownership checkpoint"))?;
        store.as_ref().context("conversation store unavailable")?.begin_raft_projection(&session, 0)?;
        let mut machine = Self {
            electorate_origin:Arc::new((0,Conf::from(&initial_conf))),
            local_id, name_vote: None, verified_genesis: None, legacy_authorized: !names.read().required,
            names: names.clone(),
            genesis: None,
            founding_barrier: FoundingBarrier::Local,
            founding_index: None,
            projected: Some((0, 0)),
            native_move: None,
            abandoned_move: None,
            owner_run:None, owner_term: 0,
            recovery_image: None,
            recovery_rows: None,
            chunked_through: 0,
            founded: false,
            policy: None,
            session,
            leader_name: default_leader,
            epoch: 0,
            closed_at_index: None,
            store,
            applied: Point::default(),
            durable: Point::default(),
            configuration: initial_conf,
            state_file,
            applied_bytes: 0,
            applied_session: None,

            in_flight_parts: BTreeMap::new(),
            prepared_image: None,
            #[cfg(test)]
            fail_next_persist: Arc::new(AtomicBool::new(false)),
        };
        // Persist initial configuration at Point(0, 0) so uncompacted crash recovery knows the
        // exact initial ConfState before replaying committed entries from convo-raft-*.log.
        machine.save_checkpoint().map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(machine)
    }

    fn same_opening(&self, left: &Genesis, right: &Genesis) -> bool {
        if left.digest != right.digest { return false; }
        if left.charter == right.charter && left.members == right.members { return true; }
        let (old, new) = if left.charter.is_none() { (left, right) } else { (right, left) };
        let n = self.names.read();
        old.charter.is_none() && new.charter.is_some() && n.pin.registrar.as_ref().is_some_and(|r| old.members.as_slice() == [node_id(r)])
            && ((n.pin.charter == new.charter && self.verified_genesis == Some(new.digest)) || n.certified(&self.session, new, false))
    }

    fn retained_owner_run(&self, img: &ConvoImage) -> Option<u64> {
        (img.epoch == self.epoch
            && img.leader == self.leader_name
            && img.genesis.as_ref().map(|g| g.digest) == self.genesis.as_ref().map(|g| g.digest))
        .then_some(self.owner_run)
        .flatten()
    }

    fn validate_image(&self, img: &ConvoImage, at: Point, replacing_provisional: bool) -> Result<(), Fatal> {
        if (self.founded && self.genesis.is_none() && img.genesis.is_some()) || (!replacing_provisional && self.genesis.as_ref().is_some_and(|prior| !img.genesis.as_ref().is_some_and(|g| self.same_opening(prior, g)))) || img.genesis.as_ref().is_some_and(|g| (img.founding_index.is_none() && !g.majority(&g.witnesses)) || g.index == 0 || g.index > at.index || g.members.is_empty() || !g.members.windows(2).all(|ids| ids.first() < ids.last())) { return Err(Fatal("snapshot conflicts with the durable session genesis")); }
        if img.owner_run==Some(0) || self.retained_owner_run(img).zip(img.owner_run).is_some_and(|(ours,theirs)|ours!=theirs) {
            return Err(Fatal("snapshot conflicts with the committed execution incarnation"));
        }
        if at.index < self.applied.index || (at.index == self.applied.index && at.term != self.applied.term) || img.founding_index.is_some_and(|i| i > at.index) || img.owner_term > at.term || img.epoch < self.epoch || img.session != self.session || img.leader.is_empty() || img.rows.iter().any(|r| !valid_row(r, &self.session, &img.leader, img.epoch)) {
            return Err(Fatal("invalid conversation snapshot frontier or custody"));
        }
        Ok(())
    }

    fn install_decoded(&mut self, img: ConvoImage, image: &[u8], at: Point, configuration: &ConfState, repair: bool) -> Result<(), Fatal> {
        if img.configuration.as_ref().is_some_and(|c|ConfState::from(c)!=*configuration) {return Err(Fatal("snapshot data and Raft electorates differ"));}
        self.electorate_origin=Arc::new((at.index,Conf::from(configuration)));
        self.owner_run=img.owner_run.or_else(||self.retained_owner_run(&img));
        self.applied = at;
        self.configuration = configuration.clone();
        self.applied_bytes = u64::try_from(image.len()).unwrap_or(0);
        self.in_flight_parts = img.in_flight_parts;
        self.applied_session = None;
        self.closed_at_index = None;
        self.chunked_through = if self.in_flight_parts.is_empty() { 0 } else { at.index };
        self.owner_term = img.owner_term;
        self.epoch = img.epoch;
        self.founded = true;
        self.genesis = img.genesis;
        self.founding_index = img.founding_index;
        self.native_move = img.native_move;
        self.abandoned_move = img.abandoned_move;
        self.policy = img.policy;
        self.leader_name = img.leader;
        let owned_rows = img.rows;
        for row in &owned_rows { self.record_frontier(row); }
        // Atomically wipe stale events, record sources, and deleted inbox keys before replaying the
        // exact snapshot rows in one SQLite transaction (`docs/durable.md` I8).
        let store = self.store.as_ref().ok_or(Fatal("conversation store unavailable during snapshot install"))?;
        let origin = self.genesis.as_ref().map(|g| g.digest);
        let replaced = store.replace_raft_rows_with_origin(&self.session, at.index, at.term, owned_rows, self.epoch, origin, repair);
        if let Err(e) = replaced {
            tracing::error!(session = %self.session, error = %format!("{e:#}"), "atomic conversation Raft snapshot install failed");
            return Err(Fatal("conversation Raft snapshot store replace failed"));
        }
        // `docs/durable.md` I8: snapshot state must be durable before returning.
        self.projected = Some((at.index, at.term));
        // Rewriting a verified repair also replaces a damaged file at the same hash path.
        let previous = if repair { self.recovery_image.take() } else { None };
        self.remember_image(image, at)?;
        if let Some(previous) = previous {
            let old = previous.path(&self.state_file);
            if self.recovery_image.as_ref().is_some_and(|current| current.path(&self.state_file) != old) { let _ = std::fs::remove_file(old); }
        }
        Ok(())
    }

    /// Keep the image that justifies WAL truncation beside its checkpoint. The pointer is durable
    /// before the old image is unlinked, so either side of a crash has a complete baseline.
    fn remember_image(&mut self, bytes: &[u8], at: Point) -> Result<(), Fatal> {
        let saved = SavedImage { content_named: true, index: at.index, term: at.term, digest: hex::encode(sha2::Sha256::digest(bytes)) };
        if self.recovery_image.as_ref().is_some_and(|old| old.index > at.index || (old.index == at.index && old.term == at.term && old.digest == saved.digest && old.path(&self.state_file).is_file())) { return Ok(()); }
        let path = saved.path(&self.state_file);
        save_atomic(&path, bytes).map_err(|_| Fatal("saving conversation recovery image failed"))?;
        let old = self.recovery_image.replace(saved);
        self.save_checkpoint()?;
        if let Some(old) = old && old.path(&self.state_file) != path {
            let _ = std::fs::remove_file(old.path(&self.state_file));
        }
        Ok(())
    }

    /// CP-T3.2. A durable peer snapshot can repair a compacted projection with no surviving
    /// local image. Preserve vote, term and fast-track promises, and never discard a suffix
    /// beyond the replacement's frontier. The checked wire envelope is a restartable journal.
    fn repair_from(&mut self, log: &mut ConvoStore, msg: &Message, local: u64, journal: &Path, record: bool) -> anyhow::Result<()> {
        let (at, image) = self.check_repair(log, msg, local)?;
        let hard = log.view()?.hard_state;
        let snap = msg.snapshot.as_deref().context("repair has no snapshot")?;
        let conf = snap.metadata.as_ref().and_then(|meta| meta.conf_state.as_ref()).context("repair configuration disappeared")?;
        if record { save_atomic(journal, &msg.encode_to_vec())?; }
        self.install_decoded(image, &snap.data, at, conf, true).map_err(|e| anyhow::anyhow!("{e}"))?;
        let new_hard = hyper_raft::proto::HardState { term: msg.term, vote: if msg.term == hard.term { hard.vote } else { 0 }, commit: at.index, ..hard };
        log.write_now(&hyper_durable::Write { start: Some(at), hard_state: Some(new_hard), ..Default::default() })?;
        std::fs::remove_file(journal)?;
        if let Some(dir) = journal.parent() { crate::blobs::chunks::sync_dir(dir)?; }
        Ok(())
    }

    fn check_repair(&self, log: &ConvoStore, msg: &Message, local: u64) -> anyhow::Result<(Point, ConvoImage)> {
        let hard = log.view()?.hard_state;
        let snap = msg.snapshot.as_deref().context("repair has no snapshot")?;
        let meta = snap.metadata.as_ref().context("repair has no metadata")?;
        let conf = meta.conf_state.as_ref().context("repair has no configuration")?;
        let image: ConvoImage = serde_json::from_slice(&snap.data)?;
        let replacing = self.founding_index.is_some() && image.founding_index.is_none() && self.genesis.as_ref().zip(image.genesis.as_ref()).is_some_and(|(ours, theirs)| {
            ours.digest != theirs.digest && ours.members == theirs.members && ours.charter == theirs.charter && self.names.read().certified(&self.session, theirs, true) && theirs.majority(&theirs.witnesses) && msg.context.strip_prefix(b"KGS1") == Some(theirs.digest.as_slice())
        });
        let prior_voter = replacing || self.configuration.voters.contains(&msg.from) || self.configuration.voters_outgoing.contains(&msg.from);
        let member = conf.voters.iter().chain(&conf.learners).chain(&conf.voters_outgoing).chain(&conf.learners_next).any(|id| *id == local);
        let source = conf.voters.contains(&msg.from) || conf.voters_outgoing.contains(&msg.from);
        anyhow::ensure!(msg.msg_type == MessageType::MsgSnapshot && msg.to == local && source && member && prior_voter && msg.term >= hard.term && meta.term <= msg.term, "repair snapshot is outside the session's voting configuration or term");
        let (base, last) = log.bounds().map_err(|e| anyhow::anyhow!("reading repair WAL bounds: {e:?}"))?;
        anyhow::ensure!(meta.index >= last.max(hard.commit).max(hard.held_index).max(self.applied.index).max(base.index) && meta.index > 0, "repair does not cover the durable session suffix");
        if meta.index == hard.commit { anyhow::ensure!(log.term(meta.index).ok() == Some(meta.term), "repair conflicts with a committed WAL term"); }
        let at = Point { index: meta.index, term: meta.term };
        self.validate_image(&image, at, replacing).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok((at, image))
    }

    /// A legacy checkpoint or a replacement database has no authoritative row baseline. Validate
    /// the complete retained history before replacing any visible rows or advancing its frontier.
    fn recover_projection(&mut self, log: &ConvoStore) -> anyhow::Result<()> {
        let store = self.store.clone().context("conversation store unavailable")?;
        let target = self.applied;
        let durable = log.view().map_err(|e| anyhow::anyhow!("reading durable conversation hard state: {e}"))?;
        if let Some((index, term)) = store.raft_projection_frontier(&self.session)?
            && store.raft_projection_origin(&self.session)? == self.genesis.as_ref().map(|g| g.digest) {
            // Two concurrent provisional roots can occupy the same Raft point. A copied
            // database belongs to this WAL only if the founding bytes match as well.
            let exact = self.projected == Some((index, term));
            // Applying can flush to SQLite before the lightweight checkpoint is replaced. A
            // later projection is reusable only if the WAL independently proves its commitment.
            let ahead = index >= target.index && index <= durable.hard_state.commit.max(durable.start.index) && log.term(index).ok() == Some(term);
            if exact || ahead { return Ok(()); }
        }
        let (base, last) = log.bounds().map_err(|e| anyhow::anyhow!("reading WAL bounds: {e:?}"))?;
        anyhow::ensure!(target.index >= base.index && target.index <= last, "checkpoint is outside retained WAL bounds");
        let current_configuration=self.configuration.clone();
        let previous_run=self.owner_run.map(|run|(self.epoch,self.leader_name.clone(),self.genesis.as_ref().map(|g|g.digest),run));
        self.owner_run=None;
        let mut historical_configuration=None;
        self.applied_session = None;
        self.closed_at_index = None;
        self.in_flight_parts.clear();
        self.founded = false;
        self.founding_index = None;
        self.native_move = None;
        self.abandoned_move = None;
        self.epoch = 0;
        self.policy = None;
        self.applied_bytes = 0;
        let (start, rows) = if let Some(saved) = &self.recovery_image {
            anyhow::ensure!(saved.index >= base.index && saved.index <= target.index && log.term(saved.index).ok() == Some(saved.term), "invalid conversation recovery image frontier");
            let bytes = std::fs::read(saved.path(&self.state_file))?;
            anyhow::ensure!(hex::encode(sha2::Sha256::digest(&bytes)) == saved.digest, "corrupt conversation recovery image");
            let image: ConvoImage = serde_json::from_slice(&bytes)?;
            anyhow::ensure!(image.session == self.session && !image.leader.is_empty() && image.rows.iter().all(|r| r.session() == self.session), "conversation recovery image belongs to another session");
            historical_configuration=image.configuration.as_ref().map(ConfState::from);
            self.applied = Point { index: saved.index, term: saved.term };
            self.founded = true;
            self.owner_run=image.owner_run.or_else(||previous_run.as_ref().filter(|(epoch,owner,origin,_)|
                *epoch==image.epoch && owner==&image.leader && *origin==image.genesis.as_ref().map(|g|g.digest)).map(|(_,_,_,run)|*run));
            self.epoch = image.epoch;
            self.leader_name = image.leader;
            anyhow::ensure!(self.genesis.as_ref().is_none_or(|prior| image.genesis.as_ref().is_some_and(|g| self.same_opening(prior, g))), "recovery image has divergent session genesis");
            self.genesis = image.genesis;
            self.founding_index = image.founding_index;
            self.native_move = image.native_move;
            self.abandoned_move = image.abandoned_move;
            self.policy = image.policy;
            self.in_flight_parts = image.in_flight_parts;
            self.owner_term = image.owner_term;
            self.applied_bytes = u64::try_from(bytes.len())?;
            for row in &image.rows { self.record_frontier(row); }
            (self.applied.index, image.rows)
        } else {
            anyhow::ensure!(base.index == 0, "compacted legacy WAL has no verified recovery image");
            self.applied = Point::default();
            (0, Vec::new())
        };
        // Restore the electorate independently of the checkpoint at the end of the
        // replay. A later configuration must never decide a historical process claim.
        let origin=self.electorate_origin.clone();
        if origin.0==start {historical_configuration=Some(ConfState::from(&origin.1));}
        self.configuration=historical_configuration.unwrap_or_default();
        self.recovery_rows = Some(rows);
        let mut expected = start;
        let mut error = None;
        let mut discard = Vec::new();
        if start < target.index {
            log.visit(start.checked_add(1).context("WAL index overflow")?, target.index.checked_add(1).context("WAL index overflow")?, APPEND_MAX, &mut |entry| {
                if expected.checked_add(1) != Some(entry.index) { error = Some(Fatal("noncontiguous conversation WAL")); return true; }
                expected = entry.index;
                error=self.apply(&entry,&mut discard).err();
                if origin.0==entry.index {
                    self.configuration=ConfState::from(&origin.1);
                } else if error.is_none() && !self.configuration.voters.is_empty() && entry.kind!=EntryType::EntryNormal {
                    error=self.replay_configuration(&entry).err();
                }
                discard.clear();
                error.is_some()
            }).map_err(|e| anyhow::anyhow!("reading conversation WAL: {e:?}"))?;
        }
        anyhow::ensure!(error.is_none() && expected == target.index && self.applied == target && self.configuration==current_configuration, "conversation WAL failed checkpoint verification: {error:?}");
        let rows = self.recovery_rows.take().context("missing recovery rows")?;
        store.replace_raft_rows_with_origin(&self.session, target.index, target.term, rows, self.epoch, self.genesis.as_ref().map(|g| g.digest), true)?;
        self.projected = Some((target.index, target.term));
        self.save_checkpoint().map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(())
    }

    fn replay_configuration(&mut self, entry: &EntryRef<'_>) -> Result<(), Fatal> {
        use hyper_raft::proto::{ConfChange, Plan, joint};
        let change = match entry.kind {
            EntryType::EntryConfChange if entry.data.is_empty() => joint(&ConfChange::default()),
            EntryType::EntryConfChange => joint(
                &ConfChange::decode(entry.data)
                    .map_err(|_| Fatal("invalid legacy WAL configuration entry"))?,
            ),
            EntryType::EntryConfChangeV2 if entry.data.is_empty() => ConfChangeV2::default(),
            EntryType::EntryConfChangeV2 => ConfChangeV2::decode(entry.data)
                .map_err(|_| Fatal("invalid WAL configuration entry"))?,
            EntryType::EntryNormal => return Ok(()),
        };
        let adjusted = (|| {
            let before = hyper_raft::Configuration::from_conf_state(&self.configuration)?;
            Plan::of(&change)?
                .apply(&before)?
                .configuration
                .to_conf_state()
        })()
        .map_err(|_| Fatal("invalid reconstructed conversation electorate"))?;
        self.configuration = adjusted;
        Ok(())
    }

    fn snapshot_spec(&self) -> anyhow::Result<SnapshotSpec> {
        let store = self.store.clone().context("conversation snapshot needs the store")?;
        Ok(SnapshotSpec {
            genesis: self.genesis.clone(),
            founding_index: self.founding_index,
            native_move: self.native_move.clone(),
            abandoned_move: self.abandoned_move.clone(),
            in_flight_parts: self.in_flight_parts.clone(),
            owner_run:self.owner_run, owner_term: self.owner_term,
            policy: self.policy,
            session: self.session.clone(),
            store,
            leader_name: self.leader_name.clone(),
            epoch: self.epoch,
            at: self.applied,
            conf: self.configuration.clone(),
            applied_session: self.applied_session.clone(),

        })
    }

    fn save_checkpoint(&mut self) -> Result<(), Fatal> {
        #[cfg(test)]
        if self.fail_next_persist.swap(false, Ordering::AcqRel) {
            return Err(Fatal("injected transient store sync failure"));
        }
        let saved = SavedConvo {
            electorate_origin:Some(self.electorate_origin.clone()),
            name_vote: self.name_vote, verified_genesis: self.verified_genesis, legacy_authorized: Some(self.legacy_authorized),
            genesis: self.genesis.clone(),
            founding_index: self.founding_index,
            founding_barrier: self.founding_barrier,
            projected: self.projected,
            native_move: self.native_move.clone(),
            abandoned_move: self.abandoned_move.clone(),
            owner_run:self.owner_run, owner_term: self.owner_term,
            recovery_image: self.recovery_image.clone(),
            chunked_through: self.chunked_through,
            founded: Some(self.founded),
            policy: self.policy,
            session: self.session.clone(),
            leader_name: self.leader_name.clone(),
            epoch: self.epoch,
            index: self.applied.index,
            term: self.applied.term,
            conf: (&self.configuration).into(),
            applied_bytes: self.applied_bytes,
            closed_at_index: self.closed_at_index,
            applied_session: self.applied_session.clone(),

            in_flight_parts: self.in_flight_parts.clone(),
        };
        let bytes = serde_json::to_vec(&saved).map_err(|_| Fatal("conversation Raft checkpoint failed to encode"))?;
        save_atomic(&self.state_file, &bytes).map_err(|_| Fatal("conversation Raft checkpoint failed to save"))?;
        self.durable = self.applied;
        Ok(())
    }

    fn record_frontier(&mut self, row: &Row) {
        match row {
            Row::Session { info, meta, state, changes, open, at, .. } => {
                let s = self.applied_session.get_or_insert_with(AppliedSession::default);
                if info.is_some() {
                    s.info.clone_from(info);
                }
                if meta.is_some() {
                    s.meta.clone_from(meta);
                }
                if state.is_some() {
                    s.state.clone_from(state);
                }
                if changes.is_some() {
                    s.changes.clone_from(changes);
                }
                s.updated_at = s.updated_at.max(*at);
                if *open {
                    s.closed_at = None;
                    self.closed_at_index = None;
                }
            }
            Row::Closed { at, .. } => {
                let s = self.applied_session.get_or_insert_with(AppliedSession::default);
                s.closed_at = Some(*at);
                self.closed_at_index = Some(self.applied.index);
            }
            _ => {}
        }
    }

    fn apply_rows(&mut self, mut entry: ConvoEntry, source_bytes: &[u8]) -> Result<bool, Fatal> {
        if ((!entry.seal_voters.is_empty() || entry.seal.is_some()) && entry.opening != Some(true)) || (!entry.genesis_members.is_empty() && entry.opening.is_none()) || entry.session != self.session || entry.leader.is_empty() || entry.epoch < self.epoch {
            return Ok(false);
        }
        // Configuration entries may have committed between collecting attestations
        // and applying this claim. Validate against this exact log position as well.
        if let Some(proof)=&entry.process_loss {
            if self.configuration.voters.is_empty() {return Err(Fatal("a custody certificate needs its original Raft electorate"));}
            let q=proof.query;
            if entry.claim_from!=Some(self.epoch) || self.epoch.checked_add(1)!=Some(entry.epoch) || !self.published()
                || !self.policy.unwrap_or_default().auto_failover || q.epoch!=self.epoch || q.term!=self.applied.term || q.owner!=node_id(&self.leader_name)
                || q.origin!=self.genesis.as_ref().map(|g|g.digest) || self.owner_run.is_some_and(|run|run!=q.run) || entry.owner_run!=Some(q.leader_run)
                || !proof.covers(&self.configuration,node_id(&entry.leader)) {return Ok(false);}
        }
        // Vote, heartbeat and catch-up transport identities can differ from the historical
        // owner of these rows. Derive ownership only from the committed log.
        if self.founded {
            let valid_owner = self.leader_name == entry.leader && self.epoch == entry.epoch;
            let next_epoch = self.epoch.checked_add(1) == Some(entry.epoch);
            // Older leaders emitted the same claim rows before the optional claim_from field.
            let legacy_claim = entry.claim_from.is_none() && entry.rows.iter().any(|row| matches!(row, Row::Machine { machine, .. } if *machine == entry.leader));
            if !valid_owner && !(next_epoch && (entry.claim_from == Some(self.epoch) || legacy_claim)) { return Ok(false); }
            if entry.claim_from.is_some_and(|previous| previous != self.epoch || !next_epoch) { return Ok(false); }
        } else if entry.claim_from.is_some() {
            return Ok(false);
        }
        if entry.rows.iter().any(|r| !valid_row(r, &self.session, &entry.leader, entry.epoch)) {
            return Ok(false);
        }
        if entry.owner_run==Some(0) || entry.owner_run.is_some() && self.founded && self.epoch==entry.epoch {return Ok(false);}
        if entry.reaffirm {
            if !self.founded || self.epoch != entry.epoch || self.leader_name != entry.leader
                || !entry.rows.is_empty() || entry.claim_from.is_some() || entry.opening.is_some()
                || entry.native_move.is_some() || entry.policy.is_some() { return Ok(false); }
            self.owner_term = self.applied.term;
            return Ok(true);
        }
        if entry.opening == Some(true) {
            if self.genesis.as_ref().map(|g| g.digest) != entry.seal { return Ok(false); }
            if !self.founded || self.owner_term != self.applied.term || self.epoch != entry.epoch || self.leader_name != entry.leader || !entry.rows.is_empty() || entry.native_move.is_some() { return Ok(false); }
            if let Some(g) = &mut self.genesis {
                if !entry.seal_voters.is_empty() {
                    let Some(charter) = entry.genesis_charter else { return Ok(false); };
                    if entry.seal_voters.len() < MIN_VOTERS || !entry.seal_voters.windows(2).all(|p| p.first() < p.last())
                        || g.charter.is_some_and(|old| old != charter || g.members != entry.seal_voters) { return Ok(false); }
                    if g.charter.is_none() {
                        // The sealed command is self-contained when the directory arrives
                        // later. Publication still checks the locally verified certificate.
                        let n = self.names.read();
                        if n.active.as_ref().is_some_and(|c| c.digest != charter || c.voters != entry.seal_voters || c.reserved.contains(&self.session)) { return Ok(false); }
                        if n.pin.registrar.as_ref().is_some_and(|r| g.members.as_slice() != [node_id(r)]) { return Ok(false); }
                    }
                    if entry.genesis_members.len() <= entry.seal_voters.len()/2
                        || !entry.genesis_members.windows(2).all(|w| w.first() < w.last())
                        || !entry.genesis_members.iter().all(|id| entry.seal_voters.binary_search(id).is_ok()) { return Ok(false); }
                    g.charter = Some(charter);
                    g.members = entry.seal_voters;
                }
                if !g.majority(&entry.genesis_members) { return Ok(false); }
                g.witnesses = entry.genesis_members;
            }
            self.founding_index = None;
            if self.recovery_rows.is_none() { self.record_name_vote()?; }
            return Ok(true);
        }
        if self.founded && entry.opening == Some(false) { return Ok(false); }
        let takeover = self.founded && self.epoch < entry.epoch;
        if let Some(intent) = entry.native_move {
            if !self.founded || self.owner_term != self.applied.term || self.epoch != entry.epoch || !entry.rows.is_empty() || !(valid_digest(&intent.digest) && !intent.leadership_only || intent.leadership_only && intent.digest.is_empty() && !intent.ready && !intent.cancel) { return Ok(false); }
            if intent.cancel {
                if intent.ready || self.epoch != intent.epoch || self.native_move.as_ref().is_some_and(|m| m.digest != intent.digest || m.target != intent.target) { return Ok(false); }
                self.native_move = None;
                self.abandoned_move = Some(intent);
                return Ok(true);
            }
            if intent.ready {
                if intent.epoch.checked_add(1) != Some(self.epoch) || intent.target != self.leader_name || !self.native_move.as_ref().is_some_and(|m| m.target == intent.target && m.epoch == intent.epoch && m.digest == intent.digest) { return Ok(false); }
            } else if self.epoch != intent.epoch || intent.target.is_empty()
                || self.native_move.as_ref().is_some_and(|prior| !prior.ready && (prior.digest != intent.digest || prior.target != intent.target || prior.epoch != intent.epoch)) {
                return Ok(false);
            }
            if self.abandoned_move.as_ref().is_some_and(|a| !intent.ready && a.target == intent.target && a.digest == intent.digest) { self.abandoned_move = None; }
            self.native_move = Some(intent);
        } else if takeover && self.native_move.as_ref().is_some_and(|m| m.leadership_only || m.epoch != self.epoch || m.target != entry.leader) {
            self.native_move = None;
        }
        let opening = !self.founded && !entry.genesis_members.is_empty();
        if opening {
            if entry.opening != Some(false) || !entry.genesis_members.windows(2).all(|w| w.first() < w.last()) { return Ok(false); }
            let genesis = Genesis { charter: entry.genesis_charter, witnesses: Vec::new(), index: self.applied.index, digest: sha2::Sha256::digest(source_bytes).into(), members: entry.genesis_members };
            if let Some(prior) = &self.genesis {
                if prior.digest != genesis.digest || (prior.members != genesis.members && !(prior.charter.is_some() && genesis.charter.is_none())) {
                    return Err(Fatal("committed history conflicts with prior session genesis"));
                }
            } else { self.genesis = Some(genesis); }
        }
        let custody_changed = !self.founded || takeover;
        if custody_changed { self.owner_run=entry.owner_run; self.owner_term = self.applied.term; }

        if !self.founded {
            self.policy = entry.policy;
            self.founding_index = (entry.opening == Some(false)).then_some(self.applied.index);
            self.founded = true;
        }
        self.epoch = entry.epoch;
        self.leader_name.clone_from(&entry.leader);
        for row in &entry.rows {
            self.record_frontier(row);
        }
        entry.rows.retain(|row| !matches!(row, Row::Machine { .. }));
        if custody_changed { entry.rows.push(Row::Machine { session: self.session.clone(), machine: entry.leader }); }
        // An empty quorum barrier certifies the durable row transactions applied before it.
        if entry.rows.is_empty() { return Ok(true); }
        self.projected = Some((self.applied.index, self.applied.term));
        if let Some(history) = &mut self.recovery_rows {
            history.append(&mut entry.rows);
            return Ok(true);
        }
        let store = self.store.as_ref().ok_or(Fatal("conversation store unavailable during apply"))?;
        let applied = if custody_changed && !takeover {
            store.replace_raft_rows_with_origin(&self.session, self.applied.index, self.applied.term, entry.rows, self.epoch, self.genesis.as_ref().map(|g| g.digest), false)
        } else if takeover {
            store.apply_raft_claim_rows(&self.session, self.applied.index, self.applied.term, self.epoch, entry.rows)
        } else {
            store.apply_raft_rows(&self.session, self.applied.index, self.applied.term, entry.rows)
        };
        applied.map_err(|err| {
            tracing::error!(session = %self.session, index = self.applied.index, error = %err, "conversation Raft projection failed");
            Fatal("conversation Raft projection failed")
        })?;
        if opening { self.record_name_vote()?; if self.durable != self.applied { self.save_checkpoint()?; } }
        Ok(true)
    }

}

impl StateMachine for ConvoMachine {
    type Answer = ApplyReceipt;

    fn apply(&mut self, entry: &EntryRef<'_>, answers: &mut Vec<ApplyReceipt>) -> Result<(), Fatal> {
        self.applied = Point { index: entry.index, term: entry.term };
        // Deterministically purge any partial multi-chunk entry stranded from an earlier Raft term
        // (e.g. a leader crashed mid-chunk) the instant any entry (including a new leader's empty
        // no-op) commits in a strictly higher term, so compaction and retirement never stay wedged.
        if !self.in_flight_parts.is_empty() {
            let before = self.in_flight_parts.len();
            self.in_flight_parts.retain(|_, p| p.term >= entry.term);
            let purged = before.saturating_sub(self.in_flight_parts.len());
            if purged > 0 {
                crate::metrics::convo_raft().dropped_proposals.add(u64::try_from(purged).unwrap_or(u64::MAX));
                tracing::warn!(session = %self.session, term = entry.term, purged, "purged incomplete multi-part Raft chunks stranded from earlier term");
            }
        }
        if entry.kind != EntryType::EntryNormal || entry.data.is_empty() {
            return Ok(());
        }
        let commit_tag = CommitTag::from_entry(entry.context, entry.term);
        self.applied_bytes = self.applied_bytes.saturating_add(u64::try_from(entry.data.len()).unwrap_or(0));
        // Decode the common whole entry directly without a second heap allocation.
        // A legacy fragment may place chunk_id after other fields.
        if !entry.data.starts_with(br#"{"chunk_id":"#)
            && let Ok(whole)=serde_json::from_slice::<ConvoEntry>(entry.data) {
            let accepted=self.apply_rows(whole,entry.data)?;
            if let Some(tag)=commit_tag {answers.push((tag,accepted,entry.index));}
            return Ok(());
        }
        match serde_json::from_slice::<WirePart<'_>>(entry.data) {
            Ok(WirePart { chunk_id, part, total, chunk_b64 }) => {
                self.chunked_through=entry.index;
                if total == 0 || part >= total {
                    crate::metrics::convo_raft().corrupt_entries.inc();
                    tracing::error!(session = %self.session, index = entry.index, chunk_id, part, total, "invalid conversation Raft chunk header");
                    return Err(Fatal("invalid conversation Raft chunk header"));
                }
                let state = self.in_flight_parts.entry(chunk_id).or_insert_with(|| InFlightPart { term: entry.term, next_part: 0, total, bytes: Arc::default() });
                if state.next_part != part || state.total != total {
                    crate::metrics::convo_raft().corrupt_entries.inc();
                    tracing::error!(session = %self.session, index = entry.index, chunk_id, expected = state.next_part, got = part, total, "out-of-order conversation Raft chunk");
                    return Err(Fatal("out-of-order conversation Raft chunk"));
                }
                // Decode into the growing command. Fallible reservation covers the codec's
                // conservative bound and avoids copying a second WAL-sized temporary.
                let bytes = Arc::make_mut(&mut state.bytes);
                let prior = bytes.len();
                bytes.try_reserve(base64::decoded_len_estimate(chunk_b64.len())).map_err(|_| Fatal("cannot reserve conversation command bytes"))?;
                if let Err(e) = base64::engine::general_purpose::STANDARD.decode_vec(chunk_b64.as_bytes(), bytes) {
                    bytes.truncate(prior);
                    crate::metrics::convo_raft().corrupt_entries.inc();
                    tracing::error!(session = %self.session, index = entry.index, chunk_id, part, error = %e, "conversation Raft chunk base64 decode failed");
                    return Err(Fatal("conversation Raft chunk base64 decode failed"));
                }
                state.next_part = state.next_part.saturating_add(1);
                if state.next_part == state.total {
                    let completed = self.in_flight_parts.remove(&chunk_id).unwrap_or_default();
                    match serde_json::from_slice::<ConvoEntry>(&completed.bytes) {
                        Ok(ce) => {
                            let accepted = self.apply_rows(ce, &completed.bytes)?;
                            if let Some(tag) = commit_tag {
                                answers.push((tag, accepted, entry.index));
                            }
                        }
                        Err(e) => {
                            crate::metrics::convo_raft().corrupt_entries.inc();
                            tracing::error!(session = %self.session, index = entry.index, chunk_id, error = %e, "reassembled conversation Raft chunk failed to decode");
                            return Err(Fatal("reassembled conversation Raft chunk failed to decode"));
                        }
                    }
                }
                Ok(())
            }
            Err(e) => {
                crate::metrics::convo_raft().corrupt_entries.inc();
                tracing::error!(session = %self.session, index = entry.index, term = entry.term, error = %e, "committed conversation Raft entry failed to decode");
                Err(Fatal("committed conversation Raft entry failed to decode"))
            }
        }
    }

    fn apply_change(&mut self, at: Point, _change: &ConfChangeV2, configuration: &ConfState) -> Result<(), Fatal> {
        self.applied = at;
        self.configuration = configuration.clone();
        Ok(())
    }

    fn durable(&self) -> Point {
        self.durable
    }

    fn configuration(&self) -> &ConfState {
        &self.configuration
    }

    fn acts_at_start(&self, entry: &EntryRef<'_>) -> bool {
        // The projection is visible to readers and an executor as soon as SQLite commits.
        !entry.data.is_empty()
    }

    fn image(&mut self, into: &mut Vec<u8>) -> Result<(Point, ConfState), Fatal> {
        if let Some((at, conf, bytes)) = self.prepared_image.take() {
            self.remember_image(&bytes, at)?;
            if into.is_empty() {
                *into = bytes;
            } else {
                into.extend_from_slice(&bytes);
            }
            return Ok((at, conf));
        }
        // Fallback when called outside off-thread preparation: build from exact committed frontier.
        let spec = self.snapshot_spec().map_err(|_| Fatal("pinning conversation snapshot failed"))?;
        let rows = spec.store.pin_raft_snapshot(&spec.session, spec.at.index).map_err(|_| Fatal("pinning conversation snapshot failed"))?;
        let bytes = build_snapshot_bytes(spec, rows).map_err(|why| {
            tracing::error!(session = %self.session, %why, "synchronous conversation Raft snapshot build failed");
            Fatal("conversation Raft snapshot build failed")
        })?;
        self.remember_image(&bytes, self.applied)?;
        if into.is_empty() {
            *into = bytes;
        } else {
            into.extend_from_slice(&bytes);
        }
        Ok((self.applied, self.configuration.clone()))
    }

    fn image_bytes(&self) -> Option<u64> {
        // Floor at one append window (`APPEND_MAX`) so tiny idle sessions don't look 0-byte and
        // trigger continuous no-progress snapshot builds on every entry.
        Some(self.applied_bytes.max(APPEND_MAX))
    }

    fn install(&mut self, image: &[u8], at: Point, configuration: &ConfState) -> Result<(), Fatal> {
        let img = serde_json::from_slice(image).map_err(|_| Fatal("conversation Raft snapshot failed to decode"))?;
        self.validate_image(&img, at, false)?;
        self.install_decoded(img, image, at, configuration, false)
    }

    fn persist(&mut self) -> Result<(), Fatal> {
        self.save_checkpoint()
    }
}

/// A queued Raft proposal storing the complete encoded `ConvoEntry` JSON (`whole`) so any
/// oversized single row (`whole.len() > ENTRY_DATA_MAX`) is lazily split into `WireEntry::Part`
/// frames bound to the current Raft term. If the replica is fenced or changes term mid-chunk,
/// `chunks` resets to `None` so the entire row is re-proposed from part 0 under a fresh `chunk_id`
/// in the new term (instead of stranding orphan later parts after term-change purge!).
#[derive(Debug, Clone)]
struct PendingProposal {
    whole: Vec<u8>,
    chunks: Option<ChunkCursor>,
    raw_no_chunk: bool,
    /// Optional commit-confirmation tag attached to `EntryRef::context` (on the final chunk if split)
    /// so `propose_sync` and failover ownership claims wait for true WAL + quorum commit + apply.
    commit_tag: Option<CommitTag>,
    /// Bytes reserved in `Manager::admission`, released ONLY when this proposal leaves
    /// `SessionGroup::pending_proposals` (proposed to Raft, rejected, or dropped on step-down/shutdown).
    reserved_bytes: usize,
}

#[derive(Debug, Clone)]
struct ChunkCursor {
    term: u64,
    id: u64,
    part: u32,
    total: u32,
}

impl PendingProposal {
    fn from_entry(whole: Vec<u8>) -> Self {
        Self { whole, chunks: None, raw_no_chunk: false, commit_tag: None, reserved_bytes: 0 }
    }

    #[cfg(test)]
    fn raw(bytes: Vec<u8>) -> Self {
        Self { whole: bytes, chunks: None, raw_no_chunk: true, commit_tag: None, reserved_bytes: 0 }
    }

    fn needs_chunking(&self) -> bool {
        !self.raw_no_chunk && self.whole.len() > ENTRY_DATA_MAX
    }

    fn reset_chunks(&mut self) {
        self.chunks = None;
    }

    /// Return the next WAL-sized frame to propose in `term` (generating a fresh `chunk_id` if this
    /// oversized entry hasn't been chunked for `term` yet), plus the `commit_tag` ONLY on the final frame.
    fn next_frame(&mut self, term: u64) -> anyhow::Result<(Vec<u8>, Option<CommitTag>)> {
        if !self.needs_chunking() {
            return Ok((self.whole.clone(), self.commit_tag));
        }
        if self.chunks.as_ref().is_none_or(|cursor| cursor.term != term) {
            let total = u32::try_from(self.whole.len().div_ceil(CHUNK_RAW_MAX))?;
            self.chunks = Some(ChunkCursor { term, id: NEXT_CHUNK_ID.fetch_add(1, Ordering::Relaxed), part: 0, total });
        }
        let cursor = self.chunks.as_ref().context("missing WAL chunk cursor")?;
        let start = usize::try_from(cursor.part)?.checked_mul(CHUNK_RAW_MAX).context("WAL chunk offset overflow")?;
        let slice = self.whole.get(start..start.saturating_add(CHUNK_RAW_MAX).min(self.whole.len())).context("invalid WAL chunk offset")?;
        #[derive(Serialize)]
        struct Chunk<'a> {
            chunk_id: u64,
            part: u32,
            total: u32,
            #[serde(serialize_with = "b64_bytes::serialize")]
            chunk_b64: &'a [u8],
        }
        let frame = Chunk { chunk_id: cursor.id, part: cursor.part, total: cursor.total, chunk_b64: slice };
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(base64::encoded_len(slice.len(), true).context("chunk size overflow")?.saturating_add(CHUNK_ENVELOPE_MAX))?;
        serde_json::to_writer(&mut bytes, &frame)?;
        anyhow::ensure!(bytes.len() <= ENTRY_DATA_MAX, "encoded WAL chunk exceeds entry bound");
        Ok((bytes, (cursor.part + 1 == cursor.total).then_some(self.commit_tag).flatten()))
    }

    fn advance(&mut self) -> bool {
        if let Some(cursor) = &mut self.chunks {
            cursor.part += 1;
            return cursor.part == cursor.total;
        }
        true
    }
}

/// Split an encoded Raft `Message` into wire frames `<= WIRE_PART_MAX` (`sync::wire::PART` = 512 KiB)
/// so arbitrarily large full-history `MsgSnapshot` frames traverse `LANE_CONVO` and direct `hyper-quic`
/// cleanly. Frames already `<= WIRE_PART_MAX` go unmodified for full append-only wire compatibility.
fn encode_wire_frames(msg_id: u64, bytes: Vec<u8>) -> Vec<Vec<u8>> {
    if bytes.len() <= WIRE_PART_MAX {
        return vec![bytes];
    }
    let payload_max = WIRE_PART_MAX.saturating_sub(WIRE_CHUNK_HDR);
    let total = u32::try_from(bytes.len().div_ceil(payload_max)).unwrap_or(u32::MAX);
    bytes
        .chunks(payload_max)
        .enumerate()
        .map(|(i, chunk)| {
            let part = u32::try_from(i).unwrap_or(u32::MAX);
            encode_wire_part(msg_id, part, total, chunk)
        })
        .collect()
}

fn encode_wire_part(id: u64, part: u32, total: u32, chunk: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(WIRE_CHUNK_HDR + chunk.len());
    frame.extend_from_slice(&WIRE_CHUNK_MAGIC);
    frame.extend_from_slice(&id.to_be_bytes());
    frame.extend_from_slice(&part.to_be_bytes());
    frame.extend_from_slice(&total.to_be_bytes());
    frame.extend_from_slice(chunk);
    frame
}

fn wal_config(align: Alignment, max_groups: usize) -> anyhow::Result<LogConfig> {
    let cache_bytes = WAL_CACHE_BYTES.min(u64::try_from(max_groups).unwrap_or(1).saturating_mul(APPEND_MAX));
    LogConfig::derive(&Facts {
        align,
        sealed: false,
        largest_entry: LARGEST_ENTRY,
        disk_bytes: SHARD_DISK_BYTES,
        max_groups,
        cadence_entries: CADENCE_ENTRIES,
        cadence_bytes: CADENCE_BYTES,
        uncommitted_entries: UNCOMMITTED_ENTRIES,
        uncommitted_bytes: UNCOMMITTED_BYTES,
        cache_bytes,
    })
    .map_err(|e| anyhow::anyhow!("conversation Raft WAL settings: {e:?}"))
}

fn open_wal_shard(dir: &Path, shard: u32, max_groups: usize) -> anyhow::Result<ConvoLog> {
    let path = shard_path(dir, shard);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let fresh = std::fs::metadata(&path).map_or(true, |m| m.len() == 0);
    let align = Alignment::new(WAL_BLOCK).map_err(|e| anyhow::anyhow!("WAL block alignment: {e:?}"))?;
    let file = DeviceFile::open(&path, true, CachingRequest::PreferDirect, align)
        .map_err(|e| anyhow::anyhow!("opening {}: {e:?}", path.display()))?;
    let cfg = wal_config(file.layout_block(), max_groups)?;
    let log_id = shard_log_id(shard);
    if fresh {
        Log::create(file, cfg, log_id).map_err(|e| anyhow::anyhow!("creating conversation Raft WAL at {}: {e:?}", path.display()))
    } else {
        Log::open(file, cfg, log_id)
            .map(|(log, _)| log)
            .map_err(|e| anyhow::anyhow!("opening conversation Raft WAL at {}: {e:?}", path.display()))
    }
}

pub(crate) fn raft_settings(id: u64, depth: usize) -> anyhow::Result<Settings> {
    let mut settings = crate::sync::group::settings(id, depth)?;
    settings.core.pre_vote = true;
    settings.core.check_quorum = true;
    settings.elections = hyper_raft::Elections::Suspicion;
    settings.disk = hyper_durable::Disk::Apart;
    Ok(settings)
}

fn conf_change(member: u64, change_type: ConfChangeType) -> ConfChangeV2 {
    ConfChangeV2 { transition: ConfChangeTransition::Auto, changes: vec![ConfChangeSingle { change_type, node_id: member }], context: Vec::new() }
}

/// Waker passed to `hyper-durable` / `hyper-log` so WAL flush completions immediately wake the owner thread.
struct Wakeup {
    session: String,
    tx: Sender<Input>,
    pending: AtomicBool,
}

impl Wakeup {
    fn new(session: &str, tx: &Sender<Input>) -> Arc<Self> {
        Arc::new(Self { session: session.to_owned(), tx: tx.clone(), pending: AtomicBool::new(false) })
    }
}

impl Wake for Wakeup {
    fn wake(self: Arc<Self>) { self.wake_by_ref(); }
    fn wake_by_ref(self: &Arc<Self>) {
        if !self.pending.swap(true, Ordering::AcqRel) {
            let _ = self.tx.send(Input::Wake(self.session.clone()));
        }
    }
}

struct OutputWake(Sender<Input>);
impl Wake for OutputWake { fn wake(self: Arc<Self>) { let _ = self.0.send(Input::OutputReady); } }

enum Input {
    OutputReady,
    NodeEvidence,
    Names(NameChange, Sender<anyhow::Result<Vec<String>>>),
    Wake(String),
    Shutdown,
    SetStore(Store),
    RegisterPeers(Vec<String>),
    RegisterMembers(Vec<(String, bool, bool)>),
    SetPeerV2(String, bool),
    SetPeerExecutor(String, bool),
    PeerUnreachable(String),
    ReportSnapshot {
        session: String,
        peer_name: String,
        arrived: bool,
    },
    OpenLeader {
        policy: Policy,
        session: String,
        epoch: u64,
        encoded: Vec<PendingProposal>,
        bytes: usize,
        reply: Option<Sender<anyhow::Result<GroupStatus>>>,
    },
    Propose {
        session: String,
        epoch: u64,
        encoded: Vec<PendingProposal>,
        bytes: usize,
        reply: Option<Sender<anyhow::Result<()>>>,
    },
    Transfer {
        digest: Option<String>,
        epoch: u64,
        session: String,
        target_node: String,
        reply: Sender<bool>,
    },
    Step {
        from_node: String,
        session: String,
        bytes: Vec<u8>,
    },
    IncomingSnapshot { from: String, session: String, peer_version: u64, charge: usize, message: Option<Box<Message>> },
    SnapshotStaged { session: String, peer: u64, charge: usize, generation: std::sync::Weak<Wakeup>, result: Result<SnapshotSend, String> },
    SnapshotPinned { session: String, generation: std::sync::Weak<Wakeup> },
    SnapshotReady {
        charge: usize,
        generation: std::sync::Weak<Wakeup>,
        session: String,
        at: Point,
        conf: ConfState,
        keep: u64,
        result: Result<Vec<u8>, String>,
    },
    #[cfg(test)]
    ProcessRefusal(String,u64,Sender<bool>),
    #[cfg(test)]
    NextTimer(Sender<Option<u64>>),
    #[cfg(test)]
    DriveDue,
    FlushWait(Sender<bool>),
    #[cfg(test)]
    ForceCompact {
        session: String,
        keep: u64,
        reply: Sender<anyhow::Result<()>>,
    },
    #[cfg(test)]
    InjectRawProposal {
        session: String,
        raw: Vec<u8>,
        reply: Sender<anyhow::Result<()>>,
    },
    #[cfg(test)]
    FailNextPersist {
        session: String,
        reply: Sender<anyhow::Result<()>>,
    },
    #[cfg(test)]
    SnapshotState {
        session: String,
        reply: Sender<(u32, bool, usize)>,
    },
}

struct OpenWaiter {
    reply: Sender<anyhow::Result<GroupStatus>>,
    admission: Arc<Mutex<AdmissionState>>,
}

impl Drop for OpenWaiter {
    fn drop(&mut self) { release_admission(&self.admission, size_of::<Self>()); }
}

struct SessionGroup {
    membership: super::liveness::Membership,
    // Process failure and storage handover are deliberately independent.
    indexed_membership: u64,
    local_disk_slow: bool,
    process_round: Option<Box<super::custody::Round>>,
    process_watch: Option<Box<super::custody::Watch>>,
    reaffirm_term: Option<u64>,
    abandon_term: Option<u64>,
    held_owner: Option<u64>,
    lost_owner: Option<u64>,
    deadline: Option<Instant>,
    rejected_witnesses: BTreeSet<u64>,
    genesis_witnesses: BTreeSet<u64>,
    sealing_term: Option<u64>,
    founding_epoch: Option<u64>,
    publication_term:Option<u64>,
    blocked_proof: usize,
    sent_proof: Option<(u64,u64,u64,bool)>,
    peer_commit: (u64, u64),
    founding_voter_commits: BTreeMap<u64, (u64, u64)>,
    applied_waiters: VecDeque<(CommitTag, u64, usize)>,
    bookkeeping_term: u64,
    // A sent v2 suffix can still be in WAL flight before machine.chunked_through advances.
    v2_suffix: bool,
    wakeup: Arc<Wakeup>,
    waker: Waker,
    replica: SessionReplica,
    wanted_learners: BTreeSet<u64>,
    /// In-flight membership change: `(member_id, promoting_to_voter, started_at)`.
    changing: Option<(u64, bool, Instant)>,
    /// Queued proposals awaiting leader election, WAL window, or stall recovery (oversized rows
    /// are lazily chunked per Raft term and reset cleanly on term change or `reopen_fenced`).
    pending_proposals: VecDeque<PendingProposal>,
    /// Callers waiting in `propose_sync` for true WAL + Raft quorum commit + state machine apply,
    /// keyed by the 8-byte `commit_tag` attached to `EntryRef::context`.
    credits: HashMap<CommitTag, usize>,
    commit_waiters: HashMap<CommitTag, Sender<anyhow::Result<()>>>,
    /// Pending automatic failover ownership claim `(claim_tag, new_epoch, term, previous_leader)`
    /// awaiting true Raft quorum commit + apply BEFORE `FailoverEvent::Claimed` is emitted.
    pending_claim: Option<(CommitTag, u64, u64, String)>,
    open_waiters: Vec<OpenWaiter>,
    /// True while a background `"kiln-convo-snap"` thread is building this group's snapshot image.
    snapshot_building: bool,
    snapshot_pinning: bool,
    snapshot_failures: u32,
    /// After a failed build or compaction, the applied index and compactable bytes it failed at:
    /// none is tried again until the log grows by the cadence that makes a compaction due
    /// (`sync::disk::due`), a caller proposes to it while it's stalled, this node's disk verdict
    /// changes, or the store is replaced. A failure is retried no more often than a group
    /// compacts, and nothing is timed.
    snapshot_held: Option<(u64, u64)>,
    /// Whether this node was the Raft leader on the previous drive pass (drives automatic failover
    /// `Claimed` and returning-leader `LostLeadership` transitions).
    was_leader: bool,
    /// Applied index when last reopened from a fence; once `applied > reopened_at_index`, reopen
    /// backoff resets cleanly.
    reopened_at_index: Option<u64>,
    #[cfg(test)]
    compact_waiters: Vec<Sender<anyhow::Result<()>>>,
    /// Set on groups reopened from WAL so we wait until committed WAL entries finish replaying
    /// before checking whether this node is still the sole voter and campaigning.
    needs_restart_campaign: bool,
    compacted: u64,
    compacting: bool,
    writes_out: usize,
}

/// Fast lock-protected read snapshot refreshed by the owner thread after each drive pass, so
/// HTTP/routing reads (`leader_of`, `status`, `all_status`, `summary`) never block on disk I/O.
#[derive(Default)]
struct Shared {
    start_holds:std::sync::Weak<crate::frontdoor::start_outbox::Outbox>,
    needs_roster: bool,
    enrolled: BTreeSet<u64>,
    custody: Option<CustodyChanges>,
    store: Option<Store>,
    statuses: BTreeMap<String, GroupStatus>,
    changed: Arc<crate::rt::Changed>,
    /// Maintained incrementally on dirty updates so `summary()` and metrics never scan all groups.
    leading_count: usize,
    #[cfg(test)]
    shard_count: usize,
    #[cfg(test)]
    wire_chunks_sent: u64,
}

#[derive(Default)]
struct CustodyChanges {
    ids: HashSet<String>,
    bytes: usize,
    rescan: bool,
}

impl Shared {
    fn note_custody(&mut self, id: &str) {
        let Some(changes) = &mut self.custody else { return };
        if changes.rescan || changes.ids.contains(id) { return; }
        let bytes = changes.bytes.saturating_add(size_of::<String>()).saturating_add(id.len());
        if bytes > crate::memory::room::share() {
            *changes = CustodyChanges { rescan: true, ..Default::default() };
        } else {
            changes.ids.insert(id.to_owned());
            changes.bytes = bytes;
        }
    }
}

#[derive(Default)]
struct RecoveryWork {
    founding_epoch: Option<u64>,
    wanted_learners: BTreeSet<u64>,
    pending: VecDeque<PendingProposal>,
    credits: HashMap<CommitTag, usize>,
    commit_waiters: HashMap<CommitTag, Sender<anyhow::Result<()>>>,
    open_waiters: Vec<OpenWaiter>,
    #[cfg(test)]
    compact_waiters: Vec<Sender<anyhow::Result<()>>>,
}

fn send_process_evidence(
    out: &crate::memory::queue::Sender<Outgoing>,
    session: &str,
    from: u64,
    to: u64,
    name: &str,
    term: u64,
    frame: super::custody::Wire,
) -> usize {
    use crate::memory::queue::Weigh as _;
    let Ok(context) = frame.context() else {
        return 0;
    };
    let mut m = hyper_raft::proto::message(to, MessageType::MsgHeartbeatResponse);
    m.from = from;
    m.term = term;
    m.context = context;
    out.try_send(Outgoing {
        session: session.to_owned(),
        to_id: to,
        to_name: name.to_owned(),
        bytes: m.encode_to_vec(),
        is_snapshot: false,
        snapshot_last: false,
    })
    .err()
    .map_or(0, |e| e.into_inner().weight())
}

impl SessionGroup {
    /// CP-T3.2. The next execution epoch starts only when this exact claim is applied.
    fn propose_claim(
        &mut self,
        session: &str,
        me: &str,
        member: u64,
        process_loss: Option<super::custody::Certificate>,
    ) -> bool {
        let machine = self.replica.machine();
        let Some(next) = machine.epoch.checked_add(1) else {
            return false;
        };
        let term = self.replica.term();
        if let Some((_, epoch, pending_term, _)) = self.pending_claim.as_ref() {
            return *epoch == next && *pending_term == term;
        }
        let rows = build_failover_claim_rows(session, me, next, machine.applied_session.as_ref());
        if !rows.iter().all(|r| valid_row(r, session, me, next)) {
            return false;
        }
        let entry = ConvoEntry {
            process_loss: process_loss.map(Box::new),
            owner_run: self
                .membership
                .shared
                .with(|l| l.observed_run(member))
                .flatten(),
            reaffirm: false,
            seal_voters: Vec::new(),
            genesis_charter: None,
            genesis_members: Vec::new(),
            seal: None,
            opening: None,
            session: session.to_owned(),
            leader: me.to_owned(),
            epoch: next,
            rows,
            policy: None,
            claim_from: Some(machine.epoch),
            native_move: None,
        };
        let Ok(bytes) = serde_json::to_vec(&entry) else {
            return false;
        };
        let tag = CommitTag::new(member, term);
        let mut proposal = PendingProposal::from_entry(bytes);
        proposal.commit_tag = Some(tag);
        self.pending_claim = Some((tag, next, term, machine.leader_name.clone()));
        self.pending_proposals.push_front(proposal);
        true
    }

    fn ownership_command(&mut self, session: &str, owner: &str, reaffirm: bool, native_move: Option<MoveIntent>) -> anyhow::Result<()> {
        let entry = ConvoEntry { process_loss:None, owner_run:None, reaffirm, seal_voters: Vec::new(), genesis_charter: None, genesis_members: Vec::new(), seal: None,
            opening: None, session: session.to_owned(), leader: owner.to_owned(), epoch: self.replica.machine().epoch,
            rows: Vec::new(), policy: None, claim_from: None, native_move };
        self.pending_proposals.push_front(PendingProposal::from_entry(serde_json::to_vec(&entry)?));
        Ok(())
    }

    fn process_verdict(&self, owner: u64) -> Option<(u64, bool)> {
        self.membership
            .shared
            .with(|l| {
                let seen = l.observed_run(owner)?;
                // A later authenticated boot is positive proof the committed execution
                // incarnation ended. Unmarked legacy epochs require a common observed run.
                let run = self.replica.machine().owner_run.unwrap_or(seen);
                Some((
                    run,
                    seen > run
                        || seen == run
                            && (l.stopped(owner)
                                || l.trust(owner) == Some(hyper_timing::Trust::Suspected)),
                ))
            })
            .flatten()
    }

    fn matches_process_query(&self, q: super::custody::Query) -> bool {
        let machine = self.replica.machine();
        self.replica.term() == q.term
            && machine.published()
            && machine.epoch == q.epoch
            && node_id(&machine.leader_name) == q.owner
            && machine.genesis.as_ref().map(|g| g.digest) == q.origin
            && machine.owner_run.is_none_or(|run| run == q.run)
            && machine.policy.unwrap_or_default().auto_failover
    }

    /// A watcher only speaks while its current leader is trying to take a different
    /// process's custody. A negative answer parks until the underlying verdict changes.
    fn answer_process_watch(
        &mut self,
        session: &str,
        peers: &BTreeMap<u64, String>,
        out: &crate::memory::queue::Sender<Outgoing>,
    ) {
        let Some(watch) = self.process_watch.as_ref() else {
            return;
        };
        let leader = watch.leader;
        let q = watch.query;
        if self.replica.leader() != leader
            || self
                .membership
                .shared
                .with(|l| l.observed_run(leader))
                .flatten()
                != Some(q.leader_run)
            || !self.matches_process_query(q)
            || !super::custody::voters(self.replica.configuration()).any(|v| v == self.replica.id())
        {
            self.process_watch = None;
            return;
        }
        let lost =
            self.replica.id() != q.owner && self.process_verdict(q.owner) == Some((q.run, true));
        let Some(watch) = self.process_watch.as_mut() else {
            return;
        };
        if watch.answered == Some(lost) {
            return;
        }
        let version = watch.serial.saturating_add(1);
        if let Some(name) = peers.get(&leader) {
            let Some(run) = self
                .membership
                .shared
                .with(|l| l.observed_run(self.replica.id()))
                .flatten()
            else {
                return;
            };
            let blocked = send_process_evidence(
                out,
                session,
                self.replica.id(),
                leader,
                name,
                q.term,
                super::custody::Wire::Answer {
                    query: q,
                    witness_run: run,
                    serial: version,
                    lost,
                },
            );
            if blocked == 0 {
                watch.answered = Some(lost);
                watch.serial = version;
            } else {
                self.blocked_proof = self.blocked_proof.max(blocked);
            }
        }
    }

    fn corroborated_loss(
        &mut self,
        session: &str,
        owner: u64,
        run: u64,
        peers: &BTreeMap<u64, String>,
        out: &crate::memory::queue::Sender<Outgoing>,
    ) -> bool {
        use super::custody::{Query, Round, Wire, voters};
        let Some(election) = self.membership.election() else {
            self.process_round = None;
            return false;
        };
        let local = self.replica.id();
        let term = self.replica.term();
        let epoch = self.replica.machine().epoch;
        let origin = self.replica.machine().genesis.as_ref().map(|g| g.digest);
        let conf = self.replica.configuration();
        let now = self.membership.shared.now_ns();
        let Some(leader_run) = self
            .membership
            .shared
            .with(|l| l.observed_run(local))
            .flatten()
        else {
            self.process_round = None;
            return false;
        };
        let new = self.process_round.as_ref().is_none_or(|r| {
            r.query.leader_run != leader_run
                || r.voters != conf.voters
                || r.outgoing != conf.voters_outgoing
                || (
                    r.query.owner,
                    r.query.epoch,
                    r.query.run,
                    r.query.term,
                    r.query.origin,
                ) != (owner, epoch, run, term, origin)
        });
        let expired = self
            .process_round
            .as_ref()
            .is_some_and(|r| r.retry.is_some_and(|t| now >= t));
        if new || expired {
            let freshness = self
                .membership
                .shared
                .with(|l| l.report(owner).and_then(|p| p.freshness))
                .flatten()
                .unwrap_or(election);
            let measured = u64::try_from(election.max(freshness).as_nanos()).unwrap_or(u64::MAX);
            let previous = self.process_round.as_deref().filter(|_| !new);
            let servicing = previous.map_or(0, |r| r.servicing);
            let window = measured.saturating_add(servicing);
            // Only retransmission backs off after a missing response. Freshness grows
            // from actual answer time and the detector; silence cannot extend a vote.
            let backoff = previous
                .filter(|r| r.asked.iter().any(|id| !r.answers.contains_key(id)))
                .map_or(window, |r| window.max(r.backoff.saturating_mul(2)));
            let q = Query {
                owner,
                run,
                epoch,
                term,
                leader_run,
                origin,
                nonce: CommitTag::new(local, term).sequence,
            };
            self.process_round = Some(Box::new(Round {
                query: q,
                voters: conf.voters.clone(),
                outgoing: conf.voters_outgoing.clone(),
                started: now,
                window,
                backoff,
                servicing,
                retry: Some(now.saturating_add(backoff)),
                asked: BTreeSet::new(),
                answers: BTreeMap::new(),
            }));
        }
        let Some(round) = &mut self.process_round else {
            return false;
        };
        let conf = self.replica.configuration();
        if round.confirmed(conf, local, now) {
            return true;
        }
        if !round.possible(conf) {
            round.retry = None;
        }
        for to in voters(conf).filter(|p| *p != local && *p != owner) {
            if round.asked.contains(&to) {
                continue;
            }
            if let Some(name) = peers.get(&to) {
                let blocked = send_process_evidence(
                    out,
                    session,
                    local,
                    to,
                    name,
                    term,
                    Wire::Check(round.query),
                );
                if blocked == 0 {
                    round.asked.insert(to);
                } else {
                    self.blocked_proof = self.blocked_proof.max(blocked);
                }
            }
        }
        false
    }

    fn has_process_quorum(&self, local:u64) -> bool {
        let c = self.replica.configuration();
        self.membership.shared.with(|l| {
            let majority = |voters:&[u64]| voters.is_empty() || voters.iter().filter(|&&v|
                v == local || matches!(l.trust(v),Some(hyper_timing::Trust::Trusted{..}))).count() > voters.len()/2;
            majority(&c.voters) && majority(&c.voters_outgoing)
        }) == Some(true)
    }

    fn conflicting_names(&self) -> bool {
        let m = self.replica.machine();
        m.founding_index.is_some() && m.genesis.as_ref().is_some_and(|g|
            self.rejected_witnesses.len() > g.members.len().saturating_sub(g.members.len()/2 + 1))
    }

    fn genesis_quorum(&self, member: u64) -> bool {
        let machine = self.replica.machine();
        let Some(g) = machine.genesis.as_ref().filter(|_| machine.founding_index.is_some()) else { return true };
        if !machine.may_publish(g) { return false; }
        let own = usize::from(g.members.binary_search(&member).is_ok() && machine.name_vote == Some(g.digest) && machine.durable.index >= g.index);
        own + self.genesis_witnesses.len() > g.members.len() / 2
    }

    /// Whether a snapshot may be built: none failed, or the log grew by a compaction's cadence since.
    fn snapshot_free(&self) -> bool {
        self.snapshot_held.is_none_or(|(index, bytes)| self.replica.applied().index.saturating_sub(index) >= CADENCE_ENTRIES / 2
            || self.replica.compactable_bytes().saturating_sub(bytes) >= CADENCE_BYTES / 2)
    }

    /// Whether a caller waits on this group's open or commit.
    fn waits(&self) -> bool { !self.open_waiters.is_empty() || !self.applied_waiters.is_empty() }

    fn receipt_frontier(&self) -> u64 {
        let conf = self.replica.configuration();
        if self.replica.machine().founding_barrier == FoundingBarrier::VotingFleet { return 0; }
        if self.replica.machine().founding_barrier == FoundingBarrier::Local && (conf.voters.len() > 1 || (conf.learners.is_empty() && self.wanted_learners.is_empty())) {
            return self.replica.durable_commit();
        }
        if self.peer_commit.0 == self.replica.term() { self.replica.durable_commit().min(self.peer_commit.1) } else { 0 }
    }
}

struct WalShard {
    log: ConvoLog,
    claimed: usize,
}

struct Owner {
    liveness: Arc<crate::liveness::Shared>,
    _subscription: crate::liveness::Subscription,
    evidence: Arc<Mutex<super::liveness::Inbox>>,
    evidence_buf: Vec<crate::liveness::Event>,
    by_peer: HashMap<u64, BTreeSet<String>>,
    deadlines: BTreeSet<(Instant,String)>,
    names: Arc<parking_lot::RwLock<NameAdmission>>,
    me: String,
    my_id: u64,
    dir: PathBuf,
    shard_groups: usize,
    shards: BTreeMap<u32, WalShard>,
    store: Option<Store>,
    peers: BTreeMap<u64, String>,
    announced: BTreeSet<u64>,
    /// Peers verified to advertise `proto::feature::CONVO_RAFT_V2` (`KRC1` chunked wire snapshots
    /// and `WireEntry::Part` oversized entries). Peers absent from this set stay on v1 framing.
    v2_peers: BTreeSet<u64>,
    // Explicit false differs from disconnected or never negotiated. Only a live
    // capability withdrawal can demote a voter in the committed configuration.
    executors: BTreeMap<u64, bool>,
    local_exec: bool,
    /// Peers currently reported unreachable so closed-session retirement never wedges on dead nodes.
    unreachable_peers: BTreeSet<u64>,
    /// Reopen attempts of each fenced group since it last committed cleanly: one fenced again
    /// before that is held rather than reopened in a loop.
    reopen_attempts: HashMap<String, u32>,
    /// Groups whose reopen failed, held until an event that could change that: a repair from a
    /// peer, a peer trusted again or restarted, a caller asking for the session, this node's
    /// disk verdict, or a new store (`release_reopens`). Nothing is timed.
    reopen_held: HashSet<String>,
    recovering: HashMap<String, RecoveryWork>,
    repair_requests: HashMap<String, (u64, u64, Instant)>,
    /// Active off-thread snapshot workers and concurrency ceiling (prevents thread explosion on restart).
    snapshots_in_flight: usize,
    snapshot_bytes: usize,
    max_snapshot_workers: usize,
    session_slots: BTreeMap<String, SessionSlot>,
    terminal: BTreeMap<String, TerminalGroup>,
    terminal_pending: BTreeSet<String>,
    terminal_retry: BTreeMap<String, (u32, Instant)>,
    groups: HashMap<String, SessionGroup>,
    /// Sessions whose `GroupStatus` changed since the last `publish_shared` (O(dirty) lock hold).
    dirty_sessions: HashSet<String>,
    /// Sessions retired/removed since the last `publish_shared`.
    removed_sessions: Vec<String>,
    /// Atomic shutdown flag + queued proposal byte reservation shared with callers.
    admission: Arc<Mutex<AdmissionState>>,
    transfers: HashMap<String, (u64, Option<String>)>,
    flush_waiters: Vec<Sender<bool>>,
    open_answers: Vec<(Sender<anyhow::Result<GroupStatus>>, anyhow::Result<GroupStatus>)>,
    commit_answers: Vec<(Sender<anyhow::Result<()>>, anyhow::Result<()>)>,
    output_place: Arc<crate::memory::queue::Place>,
    output_waker: Waker,
    waiting_output: BTreeMap<String,usize>,
    snapshot_sends: VecDeque<SnapshotSend>,
    snapshot_acks: HashMap<(String, u64), (u64, u64, Instant)>,
    wire_generation: HashMap<u64, u64>,
    wire_joiner: WireJoiner,
    next_wire_id: u64,
    #[cfg(test)]
    wire_chunks_sent: u64,
    out_tx: crate::memory::queue::Sender<Outgoing>,
    failover_tx: crate::memory::queue::Sender<FailoverEvent>,
    shared: Arc<Mutex<Shared>>,
    tx: Sender<Input>,
    scheduled: HashSet<String>,
    closed: HashSet<String>,
    // Release the process lock after every replica, log, and snapshot file.
    _wal_lock: std::fs::File,
}

impl Owner {
    #[allow(clippy::too_many_arguments)]
    fn open(
        names: Arc<parking_lot::RwLock<NameAdmission>>,
        liveness: Arc<crate::liveness::Shared>,
        me: String,
        my_id: u64,
        dir: PathBuf,
        shard_groups: usize,
        local_exec: bool,
        store: Option<Store>,
        out_tx: crate::memory::queue::Sender<Outgoing>,
        failover_tx: crate::memory::queue::Sender<FailoverEvent>,
        admission: Arc<Mutex<AdmissionState>>,
        shared: Arc<Mutex<Shared>>,
        tx: Sender<Input>,
    ) -> anyhow::Result<Self> {
        let parent = dir.parent().context("WAL directory has no parent")?;
        std::fs::create_dir_all(parent)?;
        let handover = wal_lock(&dir.with_extension("move-lock"))?;
        std::fs::create_dir_all(&dir)?;
        let owner_lock = wal_lock(&dir.join(".wal-owner.lock"))?;
        drop(handover);
        match std::fs::read(dir.join("name-admission.json")) {
            Ok(raw) => { let mut n = names.write(); n.pin = serde_json::from_slice(&raw)?; n.required = true; }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        std::fs::create_dir_all(dir.join("state")).with_context(|| format!("creating {}", dir.join("state").display()))?;
        let index_path = dir.join("sessions.json");
        let persisted_slots: BTreeMap<String, SessionSlot> = match std::fs::read(&index_path) {
            Ok(b) => serde_json::from_slice(&b).with_context(|| format!("decoding conversation Raft index {}", index_path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(anyhow::Error::from(e).context(format!("reading {}", index_path.display()))),
        };
        let archive_dir = dir.join("archive");
        std::fs::create_dir_all(&archive_dir)?;
        crate::blobs::chunks::sync_dir(&dir)?;
        let mut terminal = BTreeMap::new();
        for entry in std::fs::read_dir(&archive_dir)? {
            let path = entry?.path();
            if path.extension().is_none_or(|e| e != "json") { continue; }
            let record = std::fs::read(&path).and_then(|bytes| serde_json::from_slice::<TerminalGroup>(&bytes).map_err(std::io::Error::other));
            match record {
                Ok(record) if path.file_stem().and_then(|s| s.to_str()) == Some(&format!("{:032x}", group_id(&record.session))) => {
                    if !persisted_slots.contains_key(&record.session) { terminal.insert(record.session.clone(), record); }
                }
                _ => tracing::warn!(path = %path.display(), "terminal conversation record cannot be verified; its session remains fenced"),
            }
        }
        // Always open shard 0 plus any additional shards referenced by persisted sessions.
        let mut needed_shards: BTreeSet<u32> = BTreeSet::from([0]);
        for slot in persisted_slots.values() {
            needed_shards.insert(slot.shard);
        }
        let mut shards = BTreeMap::new();
        for shard in needed_shards {
            let log = open_wal_shard(&dir, shard, shard_groups)?;
            shards.insert(shard, WalShard { log, claimed: 0 });
        }
        let mut peers = BTreeMap::new();
        peers.insert(my_id, me.clone());
        let node_tx = tx.clone();
        let evidence: Arc<Mutex<super::liveness::Inbox>>=Arc::default();
        let pending=evidence.clone();
        let subscription = liveness.subscribe(Arc::new(move |ev| {
            if pending.lock().push(ev) { let _ = node_tx.send(Input::NodeEvidence); }
        }));
        let output_waker = Waker::from(Arc::new(OutputWake(tx.clone())));
        let mut owner = Self {
            output_waker, output_place: crate::memory::queue::Place::new(), waiting_output: BTreeMap::new(),
            liveness, _subscription: subscription, evidence, evidence_buf:Vec::new(), by_peer: HashMap::new(), deadlines: BTreeSet::new(),
            names,
            _wal_lock: owner_lock,
            me,
            my_id,
            dir: dir.clone(),
            shard_groups,
            shards,
            store,
            peers,
            announced: BTreeSet::new(),
            v2_peers: BTreeSet::from([my_id]),
            executors: BTreeMap::from([(my_id, local_exec)]),
            local_exec,
            unreachable_peers: BTreeSet::new(),
            reopen_attempts: HashMap::new(),
            reopen_held: HashSet::new(),
            recovering: HashMap::new(),
            repair_requests: HashMap::new(),
            snapshots_in_flight: 0,
            snapshot_bytes: 0,
            max_snapshot_workers: thread::available_parallelism().map(usize::from).unwrap_or(1),
            // Seed `session_slots` with every persisted slot BEFORE reopening so a transient reopen
            // error on one session NEVER erases it from `sessions.json` and orphans its intact WAL!
            session_slots: persisted_slots.clone(),
            terminal,
            terminal_pending: BTreeSet::new(),
            terminal_retry: BTreeMap::new(),
            groups: HashMap::new(),
            dirty_sessions: HashSet::new(),
            removed_sessions: Vec::new(),
            admission,
            transfers: HashMap::new(),
            flush_waiters: Vec::new(),
            open_answers: Vec::new(),
            commit_answers: Vec::new(),
            snapshot_sends: VecDeque::new(),
            snapshot_acks: HashMap::new(),
            wire_generation: HashMap::new(),
            wire_joiner: WireJoiner::staged_untimed(dir.join("incoming")),
            next_wire_id: crate::util::new_v7().as_u64_pair().1,
            #[cfg(test)]
            wire_chunks_sent: 0,
            out_tx,
            failover_tx,
            shared,
            tx,
            scheduled: HashSet::new(),
            closed: HashSet::new(),
        };
        // Reopen every conversation Raft group persisted in `convo-raft-*.log` so crash/restart
        // recovers all committed WAL entries and resumes leadership automatically.
        for (session, slot) in persisted_slots {
            if let Err(e) = owner.reopen_persisted(&session, slot) {
                owner.dirty_sessions.insert(session.clone());
                owner.recovering.insert(session.clone(), RecoveryWork::default());
                owner.reopen_attempts.insert(session.clone(), 1);
                owner.reopen_held.insert(session.clone());
                tracing::error!(%session, shard = slot.shard, gid = slot.gid, error = %format!("{e:#}"), "reopening persisted conversation Raft group failed; slot preserved in sessions.json");
            }
        }
        owner.drive_all();
        owner.publish_shared();
        Ok(owner)
    }

    fn state_path(&self, gid: u128) -> PathBuf {
        self.dir.join("state").join(format!("{gid:032x}.json"))
    }

    fn save_sessions_index(&self) -> anyhow::Result<()> {
        let path = self.dir.join("sessions.json");
        let bytes = serde_json::to_vec(&self.session_slots).context("encoding conversation Raft sessions index")?;
        save_atomic(&path, &bytes).with_context(|| format!("saving {}", path.display()))
    }

    fn claim_new_group(&mut self, session: &str) -> anyhow::Result<(SessionSlot, ConvoStore)> {
        let gid = group_id(session);
        let shard_ids: Vec<u32> = self.shards.keys().copied().collect();
        for shard_id in shard_ids {
            let Some(ws) = self.shards.get_mut(&shard_id) else { continue };
            if ws.claimed >= self.shard_groups {
                continue;
            }
            match GroupStore::claim(&ws.log, gid) {
                Ok(gs) => {
                    ws.claimed = ws.claimed.saturating_add(1);
                    return Ok((SessionSlot { shard: shard_id, gid }, gs));
                }
                Err(hyper_durable::ClaimError::Log(LogError::TooManyGroups(_))) => {
                    ws.claimed = self.shard_groups;
                    continue;
                }
                Err(e) => return Err(anyhow::anyhow!("claiming WAL group {gid} on shard {shard_id} for {session}: {e:?}")),
            }
        }
        let next_shard = self.shards.keys().next_back().copied().unwrap_or(0).saturating_add(1);
        let log = open_wal_shard(&self.dir, next_shard, self.shard_groups)?;
        let gs = GroupStore::claim(&log, gid).map_err(|e| anyhow::anyhow!("claiming WAL group {gid} on new shard {next_shard} for {session}: {e:?}"))?;
        self.shards.insert(next_shard, WalShard { log, claimed: 1 });
        Ok((SessionSlot { shard: next_shard, gid }, gs))
    }

    fn reopen_persisted(&mut self, session: &str, slot: SessionSlot) -> anyhow::Result<()> {
        let state_file = self.state_path(slot.gid);
        let ws = self.shards.get_mut(&slot.shard).ok_or_else(|| anyhow::anyhow!("missing WAL shard {} for {session}", slot.shard))?;
        let mut group_store = GroupStore::claim(&ws.log, slot.gid).map_err(|e| anyhow::anyhow!("claiming WAL group {} on shard {} for {session}: {e:?}", slot.gid, slot.shard))?;
        let depth = group_store.depth();
        let mut machine = ConvoMachine::open(session.to_owned(), self.names.clone(), self.my_id,None,self.store.clone(),state_file)?;
        let journal = machine.state_file.with_extension("peer-repair");
        let repair = if journal.try_exists()? {
            let repair = Message::decode(&std::fs::read(&journal)?).map_err(|e| anyhow::anyhow!("invalid session repair journal: {e:?}"))?;
            machine.repair_from(&mut group_store, &repair, self.my_id, &journal, false)?;
            Some(repair)
        } else { None };
        machine.recover_projection(&group_store)?;
        self.repair_requests.remove(session);
        let (base, last) = group_store.bounds().map_err(|e| anyhow::anyhow!("conversation WAL bounds: {e:?}"))?;
        let mut v2_suffix = machine.chunked_through > base.index;
        if !v2_suffix && last > machine.applied.index {
            group_store.visit(machine.applied.index.saturating_add(1), last.checked_add(1).context("WAL index overflow")?, APPEND_MAX, &mut |entry| {
                v2_suffix = entry.data.starts_with(br#"{"chunk_id":"#);
                v2_suffix
            }).map_err(|e| anyhow::anyhow!("reading unapplied WAL suffix: {e:?}"))?;
        }
        let mut replica = Replica::open(&raft_settings(self.my_id, depth)?, group_store, machine, Bytes::new(u64::try_from(crate::memory::room::share()).unwrap_or(u64::MAX)))
            .map_err(|e| anyhow::anyhow!("reopening Raft group for {session}: {e:?}"))?;
        if let Some(mut snapshot) = repair {
            // The journal restored the WAL. The reopened follower still needs the
            // authenticated leader frame to recognize it and acknowledge that WAL.
            snapshot.context.clear();
            replica.step(snapshot).context("acknowledging restored session snapshot")?;
        }
        ws.claimed = ws.claimed.saturating_add(1);
        let compacted = replica.log().bounds().map_or(0, |(base, _)| base.index);
        let recovered = self.recovering.remove(session).unwrap_or_default();
        self.session_slots.insert(session.to_owned(), slot);
        self.dirty_sessions.insert(session.to_owned());
        self.scheduled.insert(session.to_owned());
        let wakeup = Wakeup::new(session, &self.tx);
        self.groups.insert(
            session.to_owned(),
            SessionGroup {
                membership: super::liveness::Membership::new(self.liveness.clone(), slot.gid, None, self.local_exec),
                indexed_membership:0, local_disk_slow: false, process_round:None, process_watch:None, reaffirm_term: None, abandon_term: None, held_owner: None, lost_owner: None, deadline: None,
                rejected_witnesses: BTreeSet::new(), genesis_witnesses: BTreeSet::new(), sealing_term: None, founding_epoch: recovered.founding_epoch,
                publication_term:None, blocked_proof: 0, sent_proof: None, peer_commit: (0, 0), founding_voter_commits: BTreeMap::new(), applied_waiters: VecDeque::new(),
                bookkeeping_term: 0,
                v2_suffix,
                waker: Waker::from(Arc::clone(&wakeup)),
                wakeup,
                replica,
                wanted_learners: recovered.wanted_learners,
                changing: None,
                pending_proposals: recovered.pending,
                credits: recovered.credits,
                commit_waiters: recovered.commit_waiters,
                pending_claim: None,
                open_waiters: recovered.open_waiters,
                snapshot_building: false,
                snapshot_pinning: false,
                snapshot_failures: 0,
                snapshot_held: None,
                was_leader: false,
                reopened_at_index: Some(compacted),
                #[cfg(test)]
                compact_waiters: recovered.compact_waiters,
                needs_restart_campaign: true,
                compacted,
                compacting: false,
                writes_out: 0,
            },
        );
        self.link_group(session,true)?;
        Ok(())
    }

    /// Reopen a fenced replica from its durable WAL + checkpoint (`docs/durable.md` §4.3),
    /// preserving queued proposals, commit waiters, learner targets, and open waiters. Resets any
    /// partial chunk state on queued oversized rows so the entire row is re-proposed under a fresh
    /// `chunk_id` in the new term (preventing collision with term-change orphan chunk purging).
    fn reopen_fenced(&mut self, session: &str) {
        self.dirty_sessions.insert(session.to_owned());
        let Some(&slot) = self.session_slots.get(session) else { return };
        self.unlink_group(session);
        // Fenced again before it committed cleanly since its last reopen: that reopen failed too.
        let mut again = false;
        if let Some(mut old) = self.groups.remove(session) {
            again = self.reopen_attempts.contains_key(session);
            if old.was_leader {
                let _ = self.failover_tx.try_send(FailoverEvent::LostLeadership { session: session.to_owned(), new_epoch: old.replica.machine().epoch, new_leader: String::new(), term: old.replica.term() });
            }
            for p in &mut old.pending_proposals { p.reset_chunks(); }
            for (tag, _, charge) in old.applied_waiters {
                release_admission(&self.admission, charge);
                if let Some(tx) = old.commit_waiters.remove(&tag) { let _ = tx.send(Err(anyhow::anyhow!("replication proof interrupted by WAL recovery"))); }
            }
            self.recovering.insert(session.to_owned(), RecoveryWork {
                founding_epoch: old.founding_epoch,
                wanted_learners: old.wanted_learners,
                pending: old.pending_proposals,
                credits: old.credits,
                commit_waiters: old.commit_waiters,
                open_waiters: old.open_waiters,
                #[cfg(test)]
                compact_waiters: old.compact_waiters,
            });
            self.dirty_sessions.insert(session.to_owned());
            if let Some(shard) = self.shards.get_mut(&slot.shard) { shard.claimed = shard.claimed.saturating_sub(1); }
        }
        if !self.recovering.contains_key(session) || self.reopen_held.contains(session) { return; }
        let attempts = self.reopen_attempts.get(session).map_or(0, |n| *n).saturating_add(1);
        self.reopen_attempts.insert(session.to_owned(), attempts);
        let attempt = if again { Err(anyhow::anyhow!("fenced again before committing cleanly after its last reopen")) } else { self.reopen_persisted(session, slot) };
        match attempt {
            Ok(()) => { crate::metrics::convo_raft().reopened_replicas.inc(); }
            Err(err) => {
                self.reopen_held.insert(session.to_owned());
                self.answer_recovery_waiters(session);
                if let Err(e)=self.probe_saved_voters(session,slot) {tracing::debug!(%session,%e,"session repair discovery deferred");}
                tracing::warn!(%session, error = %err, attempts, "conversation WAL reopen failed; it waits for a repair, a peer's return, a caller, its disk or a new store");
            }
        }
    }

    /// An event that could change a failed reopen's outcome: each held group is tried once more.
    fn release_reopens(&mut self) {
        for session in std::mem::take(&mut self.reopen_held) { self.reopen_fenced(&session); }
    }

    /// Whoever waits on a group whose reopen failed is told now. What they asked for stays queued
    /// for when it reopens, so whether it commits is decided then.
    fn answer_recovery_waiters(&mut self, session: &str) {
        let Some(work) = self.recovering.get_mut(session) else { return };
        for waiter in work.open_waiters.drain(..) {
            self.open_answers.push((waiter.reply.clone(), Err(anyhow::anyhow!("session {session}'s WAL is being repaired; its ownership isn't confirmed"))));
        }
        for (_, tx) in work.commit_waiters.drain() {
            let _ = tx.send(Err(anyhow::anyhow!("session {session}'s WAL is being repaired; whether this commits is decided once it reopens")));
        }
    }

    /// Spawn an off-thread snapshot build for `session` so the shared owner thread never blocks on
    /// SQLite reads or JSON encoding while other sessions are heartbeating/committing, bounded by
    /// `max_snapshot_workers` so simultaneous compactions after a restart never explode OS threads.
    fn spawn_snapshot_build(&mut self, session: &str, keep: u64, force: bool) {
        if !force && self.snapshots_in_flight >= self.max_snapshot_workers {
            return;
        }
        let Some(sg) = self.groups.get_mut(session) else { return };
        if sg.snapshot_building { return; }
        // SQLite rows and their encoded image coexist. Use this group's measured encoded state
        // size and process headroom; one oversized group can still progress on its own.
        let charge = usize::try_from(sg.replica.machine().applied_bytes).unwrap_or(usize::MAX).saturating_mul(2);
        if !force && self.snapshots_in_flight != 0 && charge.saturating_add(self.snapshot_bytes) > crate::memory::room::share() { return; }
        self.snapshot_bytes = self.snapshot_bytes.saturating_add(charge);
        sg.snapshot_building = true;
        sg.snapshot_pinning = true;
        self.snapshots_in_flight = self.snapshots_in_flight.saturating_add(1);
        let generation = Arc::downgrade(&sg.wakeup);
        let spec = match sg.replica.machine().snapshot_spec() {
            Ok(spec) => spec,
            Err(err) => {
                let at = sg.replica.applied();
                let conf = sg.replica.configuration().clone();
                self.on_snapshot_ready(session, generation, at, conf, keep, charge, Err(format!("pinning snapshot: {err:#}")));
                return;
            }
        };
        let tx = self.tx.clone();
        let name = session.to_owned();
        if let Err(e) = thread::Builder::new().name("kiln-convo-snap".into()).spawn(move || {
            let at = spec.at;
            let conf = spec.conf.clone();
            let pinned = spec.store.pin_raft_snapshot(&spec.session, spec.at.index).map_err(|e| format!("pinning snapshot: {e:#}"));
            if tx.send(Input::SnapshotPinned { session: name.clone(), generation: generation.clone() }).is_err() { return; }
            let result = pinned.and_then(|rows| build_snapshot_bytes(spec, rows));
            let _ = tx.send(Input::SnapshotReady { session: name, generation, at, conf, keep, charge, result });
        }) {
            self.snapshot_bytes = self.snapshot_bytes.saturating_sub(charge);
            self.snapshots_in_flight = self.snapshots_in_flight.saturating_sub(1);
            tracing::error!(%session, error = %e, "spawning off-thread snapshot builder failed");
            if let Some(sg) = self.groups.get_mut(session) {
                sg.snapshot_building = false;
                sg.snapshot_pinning = false;
                sg.snapshot_failures = sg.snapshot_failures.saturating_add(1);
                sg.snapshot_held = Some((sg.replica.applied().index, sg.replica.compactable_bytes()));
            }
        }
    }

    fn stage_snapshot(&mut self, job: SnapshotJob) {
        let Some(sg) = self.groups.get_mut(&job.session) else { return };
        let charge = job.message.encoded_len().saturating_mul(2);
        let peer = job.message.to;
        if self.snapshots_in_flight >= self.max_snapshot_workers || (self.snapshots_in_flight != 0 && self.snapshot_bytes.saturating_add(charge) > crate::memory::room::share()) {
            let _ = sg.replica.report_snapshot(peer, false);
            return;
        }
        let session = job.session.clone();
        let generation = Arc::downgrade(&sg.wakeup);
        let tx = self.tx.clone();
        // The wire codec requires a contiguous envelope. Keep its allocation, checksum and
        // spool I/O off the thread responsible for other sessions' heartbeats.
        let started = thread::Builder::new().name("kiln-convo-wire".into()).spawn(move || {
            let result = job.stage().map_err(|e| format!("{e:#}"));
            let _ = tx.send(Input::SnapshotStaged { session, peer, generation, charge, result });
        });
        match started {
            Ok(_) => {
                self.snapshot_bytes = self.snapshot_bytes.saturating_add(charge);
                self.snapshots_in_flight += 1;
            }
            Err(e) => { tracing::warn!(%e, "starting snapshot wire worker failed"); let _ = sg.replica.report_snapshot(peer, false); }
        }
    }

    /// Free room on shard `shard` by resuming every stalled group on that shard after a compaction
    /// or group retirement frees log segments.
    fn resume_shard(&mut self, shard: u32) {
        let sessions: Vec<String> = self
            .session_slots
            .iter()
            .filter(|(_, slot)| slot.shard == shard)
            .map(|(s, _)| s.clone())
            .collect();
        for s in sessions {
            if let Some(sg) = self.groups.get_mut(&s)
                && sg.replica.is_stalled()
            {
                sg.replica.resume();
                self.scheduled.insert(s);
            }
        }
    }

    fn archive_path(&self, session: &str) -> PathBuf {
        self.dir.join("archive").join(format!("{:032x}.json", group_id(session)))
    }

    fn restore_terminal(&mut self, session: &str) -> anyhow::Result<()> {
        if self.session_slots.contains_key(session) { self.terminal_retry.remove(session); return Ok(()); }
        let record = self.terminal.get(session).cloned().context("missing terminal record")?;
        let checkpoint_path = self.state_path(group_id(session));
        let saved: SavedConvo = serde_json::from_slice(&std::fs::read(&checkpoint_path)?)?;
        let anchor = saved.recovery_image.context("terminal session lacks recovery image")?;
        anyhow::ensure!(saved.session == session && saved.closed_at_index.is_some() && (saved.index, saved.term) == (record.index, record.term) && (anchor.index, anchor.term) == (record.index, record.term), "terminal checkpoint does not match archive");
        let hard = hyper_raft::proto::HardState::decode(&record.hard).map_err(|e| anyhow::anyhow!("decoding terminal Raft hard state: {e:?}"))?;
        anyhow::ensure!(hard.commit == record.index && hard.term >= record.term, "invalid terminal hard state");
        let (slot, mut log) = self.claim_new_group(session)?;
        let restored = (|| -> anyhow::Result<()> {
            let view = log.view()?;
            let at = Point { index: record.index, term: record.term };
            if view.last == 0 {
                log.write_now(&hyper_durable::Write { start: Some(at), hard_state: Some(hard), ..Default::default() })?;
            } else {
                anyhow::ensure!(view.last == record.index && log.term(view.last).ok() == Some(record.term) && view.hard_state == hard, "unexpected existing WAL at terminal restore");
            }
            Ok(())
        })();
        drop(log);
        if let Some(ws) = self.shards.get_mut(&slot.shard) { ws.claimed = ws.claimed.saturating_sub(1); }
        restored?;
        self.session_slots.insert(session.to_owned(), slot);
        if let Err(e) = self.save_sessions_index() { self.session_slots.remove(session); return Err(e); }
        self.recovering.entry(session.to_owned()).or_default().wanted_learners.extend(self.peers.keys().copied().filter(|id| !record.members.contains(id)));
        if let Err(e) = self.reopen_persisted(session, slot) {
            self.reopen_attempts.insert(session.to_owned(), 1);
            self.reopen_held.insert(session.to_owned());
            return Err(e);
        }
        self.terminal.remove(session);
        self.terminal_retry.remove(session);
        Ok(())
    }

    /// Retire a closed conversation Raft group once every member holds the close entry durably:
    /// preserves its terminal image, checkpoint and vote before freeing the active WAL slot.
    fn retire_group(&mut self, session: &str) {
        let Some(&slot) = self.session_slots.get(session) else { return };
        let Some(group) = self.groups.get(session) else { return };
        let at = group.replica.applied();
        let Ok(view) = group.replica.log().view() else { return };
        if view.last != at.index || view.hard_state.commit != at.index || view.health != hyper_durable::Health::Whole { return; }
        let conf = group.replica.configuration();
        let record = TerminalGroup { session: session.to_owned(), index: at.index, term: at.term, hard: view.hard_state.encode_to_vec(), members: conf.voters.iter().chain(&conf.learners).copied().collect() };
        let archive = self.archive_path(session);
        if let Err(e) = serde_json::to_vec(&record).map_err(std::io::Error::other).and_then(|bytes| save_atomic(&archive, &bytes)) {
            tracing::warn!(%session, %e, "saving terminal conversation archive failed");
            return;
        }
        self.session_slots.remove(session);
        if let Err(e) = self.save_sessions_index() {
            self.session_slots.insert(session.to_owned(), slot);
            tracing::warn!(%session, %e, "saving terminal WAL slot release failed");
            return;
        }
        self.terminal.insert(session.to_owned(), record);
        self.unlink_group(session);
        let Some(old) = self.groups.remove(session) else { return };
        for (_, tx) in old.commit_waiters {
            let _ = tx.send(Err(anyhow::anyhow!("session Raft group {session} retired before commit")));
        }
        drop(old.replica);
        if let Some(ws) = self.shards.get_mut(&slot.shard) {
            if let Err(e) = GroupStore::remove(&ws.log, slot.gid) {
                tracing::warn!(%session, shard = slot.shard, gid = slot.gid, error = ?e, "removing retired conversation WAL group failed");
            }
            ws.claimed = ws.claimed.saturating_sub(1);
        }
        self.transfers.remove(session);
        self.closed.remove(session);
        self.reopen_attempts.remove(session); self.reopen_held.remove(session);
        self.dirty_sessions.remove(session);
        self.removed_sessions.push(session.to_owned());
        crate::metrics::convo_raft().retired_groups.inc();
        self.resume_shard(slot.shard);
    }

    fn retire_closed_groups(&mut self) {
        let mut to_retire = Vec::new();
        let mut to_archive = Vec::new();
        for session in &std::mem::take(&mut self.closed) {
            let Some(sg) = self.groups.get_mut(session) else { continue };
            let Some(close_idx) = sg.replica.machine().closed_at_index else {
                continue;
            };
            if sg.replica.fenced().is_some()
                || !sg.replica.machine().in_flight_parts.is_empty()
                || sg.writes_out > 0
                || sg.snapshot_building || sg.compacting || sg.changing.is_some() || !sg.wanted_learners.is_empty()
                || !sg.replica.configuration().voters_outgoing.is_empty()
                || !sg.pending_proposals.is_empty()
                || !sg.credits.is_empty()
                || !sg.commit_waiters.is_empty()
                || sg.pending_claim.is_some()
                || !sg.open_waiters.is_empty()
            {
                continue;
            }
            if sg.replica.durable_commit() < close_idx || sg.replica.applied().index < close_idx {
                continue;
            }
            // Never remove the WAL while final rows still await SQLite commit: force a checkpoint
            // sync past `close_idx` first and abort retirement if SQLite failed/lost a batch!
            if sg.replica.machine().durable.index < close_idx
                && let Err(cause) = sg.replica.machine_mut().save_checkpoint()
            {
                tracing::warn!(%session, %cause, "deferring closed-group retirement because SQLite checkpoint has not synced past close");
                continue;
            }
            if sg.replica.is_leader() {
                let conf = sg.replica.configuration();
                // Retire only when every configured peer has confirmed holding the
                // close entry durably — never using an arbitrary wall-clock timeout to drop slow
                // connected peers behind!
                let all_reachable_caught_up = conf
                    .voters
                    .iter()
                    .chain(conf.learners.iter())
                    .copied()
                    .filter(|&id| id != self.my_id)
                    .all(|id| sg.replica.caught_up(id));
                if all_reachable_caught_up {
                    if sg.replica.machine().recovery_image.as_ref().is_some_and(|image| image.index == sg.replica.applied().index) {
                        to_retire.push(session.clone());
                    } else if sg.snapshot_free() {
                        to_archive.push(session.clone());
                    }
                }
            }
        }
        for s in to_archive { self.spawn_snapshot_build(&s, 0, false); }
        for s in to_retire {
            self.retire_group(&s);
        }
    }

    fn run(mut self, rx: Receiver<Input>) {
        let mut drive_time = Duration::ZERO;
        loop {
            let next = self.next_deadline();
            let received = match next {
                Some(at) => rx.recv_timeout(at.saturating_duration_since(Instant::now())),
                None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match received {
                Ok(input) if !matches!(input, Input::Shutdown) => {
                    if !self.handle(input) { self.drain_before_shutdown(&rx); return; }
                }
                Ok(_) | Err(RecvTimeoutError::Disconnected) => { self.drain_before_shutdown(&rx); return; }
                Err(RecvTimeoutError::Timeout) => {}
            }
            let until = Instant::now() + drive_time;
            let until = next.map_or(until, |at| at.min(until));
            while Instant::now() < until {
                let Ok(input) = rx.try_recv() else { break };
                if !self.handle(input) { self.drain_before_shutdown(&rx); return; }
            }
            self.on_deadline();
            let driving = Instant::now();
            if let Some(session) = self.terminal_pending.pop_first() && let Err(e) = self.restore_terminal(&session) {
                let attempt = self.terminal_retry.get(&session).map_or(1, |(n, _)| n.saturating_add(1));
                self.terminal_retry.insert(session.clone(), (attempt, Instant::now() + crate::events::backoff(i64::from(attempt))));
                tracing::warn!(%session, %e, "restoring terminal conversation for new member failed");
            }
            self.drive_all();
            self.retire_closed_groups();
            drive_time = driving.elapsed();
            self.publish_shared();
            self.notify_flush_waiters();
        }
    }

    /// Flush queued proposals and in-flight WAL writes cleanly before closing `convo-raft-*.log`
    /// on shutdown/hand-over so already-queued proposals persist durably instead of disappearing.
    fn drain_before_shutdown(&mut self, rx: &Receiver<Input>) {
        // First absorb any proposals/opens that were already queued in `rx` before `Input::Shutdown`.
        while let Ok(input) = rx.try_recv() {
            match input {
                Input::Propose { .. } | Input::OpenLeader { .. } | Input::SnapshotPinned { .. } | Input::SnapshotReady { .. } | Input::Step { .. } => {
                    let _ = self.handle(input);
                }
                i @ Input::Wake(_) => { self.handle(i); }
                _ => {}
            }
        }
        // Driven by events alone: each WAL write's completion wakes its group (`Input::Wake`), and the drain ends once
        // nothing is out, or once the node's own disk is judged slow: the liveness detector's verdict on the writes the
        // groups report out (`convo::liveness`, `report_io`), which comes as evidence (`Event::LocalDisk`).
        loop {
            self.drive_all();
            self.publish_shared();
            self.notify_flush_waiters();
            let pending = self.groups.values().any(|sg| sg.writes_out > 0 || (sg.replica.is_leader() && !sg.pending_proposals.is_empty()));
            if !pending || self.liveness.with(|l| l.own_disk_slow()) == Some(true) {
                break;
            }
            match rx.recv() {
                Ok(i @ (Input::Wake(_) | Input::SnapshotPinned { .. } | Input::NodeEvidence)) => { self.handle(i); }
                Ok(Input::SnapshotReady { session, generation, at, conf, keep, charge, result }) => {
                    self.scheduled.insert(session.clone());
                    self.on_snapshot_ready(&session, generation, at, conf, keep, charge, result);
                }
                Ok(i @ (Input::Propose { .. } | Input::OpenLeader { .. } | Input::Step { .. })) => {
                    let _ = self.handle(i);
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        for (session, sg) in &mut self.groups {
            if !sg.pending_proposals.is_empty() {
                let n = u64::try_from(sg.pending_proposals.len()).unwrap_or(u64::MAX);
                let released: usize = sg.pending_proposals.drain(..).map(|p| p.reserved_bytes).sum();
                release_admission(&self.admission, released);
                crate::metrics::convo_raft().dropped_proposals.add(n);
                tracing::warn!(%session, queued = n, "conversation Raft shut down with unproposed entries still queued");
            }
            for (_, tx) in sg.commit_waiters.drain() {
                let _ = tx.send(Err(anyhow::anyhow!("conversation Raft shut down before commit for {session}")));
            }
            release_admission(&self.admission, sg.applied_waiters.drain(..).map(|(_,_,bytes)| bytes).sum());
            release_admission(&self.admission, sg.credits.drain().map(|(_,bytes)| bytes).sum());
            sg.pending_claim = None;
        }
    }

    /// Custody check before lazily creating a peer-led replica for an unknown `session`:
    /// 1. Refuses stray non-replication frames.
    /// 2. Refuses a peer originating a session whose `Row::Session` `info.target` explicitly names
    ///    a different runner (prevents third-party session impersonation).
    /// 3. Refuses overwriting a session this node's own `Store` already owns locally unless the
    ///    incoming Raft frame carries a strictly higher epoch.
    fn peer_may_initiate(&self, session: &str, from_node: &str, msg: &Message) -> bool {
        if !matches!(
            msg.msg_type,
            MessageType::MsgAppend | MessageType::MsgHeartbeat | MessageType::MsgSnapshot | MessageType::MsgRequestVote | MessageType::MsgRequestPreVote
        ) {
            return false;
        }
        // Reject any peer trying to initiate a group whose initial `Row::Session.info.target`
        // explicitly names a DIFFERENT runner than authenticated `from_node`.
        // Borrow the routing fields and skip event text, transcripts, attachments and
        // inbox values entirely. Their single full decode belongs to committed apply.
        #[derive(Deserialize)]
        struct Scope<'a> {
            #[serde(borrow)] session: std::borrow::Cow<'a,str>,
            #[serde(borrow)] leader: std::borrow::Cow<'a,str>,
            #[serde(default)] epoch: u64,
            #[serde(borrow)] rows: Vec<RowScope<'a>>,
        }
        #[derive(Deserialize)]
        struct RowScope<'a> {
            #[serde(borrow)] r: std::borrow::Cow<'a,str>,
            #[serde(borrow)] session: std::borrow::Cow<'a,str>,
            #[serde(borrow, default)] info: Option<std::borrow::Cow<'a,str>>,
            #[serde(default)] epoch: Option<u64>,
            #[serde(borrow, default)] family: Option<std::borrow::Cow<'a,str>>,
            #[serde(borrow, default)] key: Option<std::borrow::Cow<'a,str>>,
        }
        let mut incoming_epoch = None;
        for entry in &msg.entries {
            if entry.entry_type != EntryType::EntryNormal || entry.data.is_empty() { continue; }
            let Ok(ce) = serde_json::from_slice::<Scope<'_>>(&entry.data) else {
                if !serde_json::from_slice::<WirePart<'_>>(&entry.data).is_ok_and(|p| p.total > 0 && p.part < p.total) { return false; }
                continue;
            };
            if ce.session != session || ce.leader.is_empty() || (entry.term == msg.term && ce.leader != from_node) { return false; }
            for row in ce.rows {
                if row.session != session { return false; }
                match row.r.as_ref() {
                    "session" if row.epoch.is_some_and(|sent| sent != ce.epoch) || info_violates_sender_custody(row.info.as_deref(), &ce.leader) => return false,
                    "inbox" => {
                        let (Some(family), Some(key)) = (row.family, row.key) else { return false; };
                        let Ok(key) = base64::engine::general_purpose::STANDARD.decode(key.as_bytes()) else { return false; };
                        if (crate::harness::inbox::FAMILIES.whose)(&family, &key) != Some(session.as_bytes()) { return false; }
                    }
                    "session" | "event" | "reset" | "closed" | "record" | "record_reset" | "machine" => {}
                    _ => return false,
                }
            }
            incoming_epoch = Some(incoming_epoch.map_or(ce.epoch, |epoch: u64| epoch.max(ce.epoch)));
        }
        let Some(store) = &self.store else { return false; };
        let kept = match store.kept(session, 0) {
            Ok(Some(kept)) => kept,
            Ok(None) => return true,
            Err(_) => return false,
        };
        if kept.closed_at.is_some() {
            return true;
        }
        let info_target = kept
            .info
            .as_deref()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .and_then(|v| v.get("target").and_then(serde_json::Value::as_str).map(str::to_owned));
        let owned_locally = info_target
            .as_deref()
            .map(|t| t.strip_prefix("agent:").unwrap_or(t))
            .is_some_and(|t| t == self.me || t == "local");
        if !owned_locally || from_node == self.me {
            return true;
        }
        let local_epoch = kept
            .state
            .as_deref()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .and_then(|v| v.get("epoch").and_then(serde_json::Value::as_u64))
            .unwrap_or(0);
        // Serde skips row bodies, partial commands and native data in a snapshot at admission.
        // The state machine verifies its complete contents at installation.
        #[derive(Deserialize)]
        struct SnapshotEpoch<'a> { session: &'a str, epoch: u64 }
        let incoming_epoch = incoming_epoch.or_else(|| {
            let image = serde_json::from_slice::<SnapshotEpoch<'_>>(&msg.snapshot.as_ref()?.data).ok()?;
            (image.session == session).then_some(image.epoch)
        }).unwrap_or(0);
        incoming_epoch > local_epoch
    }

    #[allow(clippy::too_many_arguments)]
    fn on_snapshot_ready(&mut self, session: &str, generation: std::sync::Weak<Wakeup>, at: Point, conf: ConfState, keep: u64, charge: usize, result: Result<Vec<u8>, String>) {
        self.snapshot_bytes = self.snapshot_bytes.saturating_sub(charge);
        self.snapshots_in_flight = self.snapshots_in_flight.saturating_sub(1);
        let now = self.now_ns();
        let mut fenced = false;
        let mut compacted_ok = false;
        let mut compact_res: anyhow::Result<()> = Ok(());
        if let Some(sg) = self.groups.get_mut(session) && generation.ptr_eq(&Arc::downgrade(&sg.wakeup)) {
            sg.snapshot_building = false;
            sg.snapshot_pinning = false;
            match result {
                Ok(bytes) => {
                    crate::metrics::convo_raft().snapshots_built.inc();
                    sg.replica.machine_mut().prepared_image = Some((at, conf, bytes));
                    // Never advance log start past `at.index` even if new entries applied while
                    // the background thread read Store (`snapshot_index >= log_start` invariant).
                    let effective_keep = sg.replica.applied().index.saturating_sub(at.index).max(keep);
                    match sg.replica.compact(effective_keep, now, &sg.waker) {
                        Ok(true) => {
                            sg.compacting = true;
                            sg.snapshot_failures = 0;
                            sg.snapshot_held = None;
                            compacted_ok = true;
                        }
                        Ok(false) => {
                            // Compaction was skipped (already at start or moving): discard unused
                            // prepared image so a later compact builds fresh instead of reusing stale bytes.
                            sg.replica.machine_mut().prepared_image = None;
                            sg.snapshot_failures = 0;
                            sg.snapshot_held = None;
                        }
                        Err(ReplicaError::Fenced(cause)) => {
                            sg.replica.machine_mut().prepared_image = None;
                            tracing::error!(%session, %cause, "conversation Raft replica fenced during compact");
                            fenced = true;
                            compact_res = Err(anyhow::anyhow!("fenced during compact: {cause}"));
                        }
                        Err(e) => {
                            sg.replica.machine_mut().prepared_image = None;
                            sg.snapshot_failures = sg.snapshot_failures.saturating_add(1);
                            sg.snapshot_held = Some((sg.replica.applied().index, sg.replica.compactable_bytes()));
                            tracing::warn!(%session, error = %e, failures = sg.snapshot_failures, "conversation Raft compact deferred with backoff");
                            compact_res = Err(anyhow::anyhow!("compact deferred: {e}"));
                        }
                    }
                }
                Err(why) => {
                    sg.snapshot_failures = sg.snapshot_failures.saturating_add(1);
                    sg.snapshot_held = Some((sg.replica.applied().index, sg.replica.compactable_bytes()));
                    tracing::warn!(%session, %why, failures = sg.snapshot_failures, "off-thread conversation Raft snapshot deferred with backoff");
                    compact_res = Err(anyhow::anyhow!(why));
                }
            }
            #[cfg(test)]
            for tx in sg.compact_waiters.drain(..) {
                let res = match &compact_res {
                    Ok(()) => Ok(()),
                    Err(e) => Err(anyhow::anyhow!("{e:#}")),
                };
                let _ = tx.send(res);
            }
            let _ = compact_res;
        }
        if compacted_ok
            && let Some(&slot) = self.session_slots.get(session)
        {
            self.resume_shard(slot.shard);
        }
        if fenced {
            self.reopen_fenced(session);
        }
    }

    fn change_names(&mut self, change: NameChange) -> anyhow::Result<Vec<String>> {
        let inventory = || self.session_slots.keys().chain(self.terminal.keys()).cloned().collect();
        if matches!(change, NameChange::Inventory) {
            anyhow::ensure!(self.names.read().pin.registrar.is_some(), "bind the authenticated name registrar before exporting historical names");
            return Ok(inventory());
        }
        let mut n = self.names.write();
        let mut pin = n.pin.clone();
        let mut charter = None;
        let mut historical = None;
        let freeze = matches!(change, NameChange::Freeze);
        match change {
            NameChange::Registrar(name) => {
                anyhow::ensure!(!name.is_empty() && pin.registrar.as_ref().is_none_or(|known| *known == name), "another fleet name registrar is already pinned");
                pin.registrar = Some(name);
            }
            NameChange::Historical { incomplete, reserved } => {
                pin.needs_inventory = true;
                historical = Some((!incomplete).then_some(reserved));
            }
            NameChange::Freeze => {
                anyhow::ensure!(!pin.needs_inventory || n.historical.is_some(), "all holders must supply their pre-upgrade names before the registrar delegates");
                anyhow::ensure!(pin.registrar.as_deref() == Some(&self.me), "only the original name registrar may freeze its namespace");
                if !pin.frozen {
                    for (session, slot) in &self.session_slots {
                        // Re-read the durable checkpoint for fenced slots. If unavailable, the
                        // owner must restore it before delegating this namespace.
                        let active = self.groups.get(session).filter(|sg| sg.replica.fenced().is_none());
                        let origin = if let Some(sg) = active {
                            let m = sg.replica.machine();
                            (!m.founded || m.founding_index.is_some()).then(|| FrozenOrigin {
                                selected: m.leader_name.clone(), digest: m.genesis.as_ref().map(|g| g.digest),
                            })
                        } else {
                            let saved: SavedConvo = serde_json::from_slice(&std::fs::read(self.state_path(slot.gid))?)
                                .with_context(|| format!("restore the original WAL checkpoint for {session} before delegating names"))?;
                            anyhow::ensure!(saved.session == *session, "the frozen slot for {session} belongs to another history");
                            (!saved.founded.unwrap_or(saved.index > 0) || saved.founding_index.is_some()).then(|| FrozenOrigin {
                                selected: saved.leader_name, digest: saved.genesis.map(|g| g.digest),
                            })
                        };
                        if let Some(origin) = origin {
                            anyhow::ensure!(!origin.selected.is_empty(), "recover the original selected node for {session} before delegating names");
                            pin.pending.insert(session.clone(), origin);
                        }
                    }
                    pin.frozen = true;
                }
            }
            NameChange::Inventory => return Ok(inventory()),
            NameChange::Require(digest) => {
                anyhow::ensure!(pin.registrar.as_deref() != Some(&self.me) || pin.frozen, "the original registrar must capture its unfinished slots before binding its charter");
                anyhow::ensure!(pin.registrar.is_some() && pin.charter.is_none_or(|old| old == digest), "the fleet charter lacks a positive handoff from its pinned authority");
                pin.frozen = true;
                pin.charter = Some(digest);
            }
            NameChange::Install(next) => {
                anyhow::ensure!(pin.registrar.is_some() && pin.charter.is_none_or(|old| old == next.digest), "the original registrar and immutable charter must match");
                anyhow::ensure!(pin.registrar.as_deref() != Some(&self.me) || pin.frozen, "registrar has not durably frozen its earlier namespace");
                if let Some(old) = &n.active {
                    anyhow::ensure!(old.digest == next.digest && old.voters == next.voters && old.reserved == next.reserved, "conflicting certified namespace charter");
                }
                pin.charter = Some(next.digest);
                pin.frozen = true;
                charter = Some(next);
            }
        }
        if pin != n.pin { save_atomic(&self.dir.join("name-admission.json"), &serde_json::to_vec(&pin)?)?; }
        n.pin = pin;
        n.required = true;
        if let Some(charter) = charter { n.active = Some(charter); }
        if let Some(historical) = historical { n.historical = historical; }
        drop(n);
        self.scheduled.extend(self.groups.keys().cloned());
        self.dirty_sessions.extend(self.groups.keys().cloned());
        if freeze { return Ok(inventory()); }
        Ok(Vec::new())
    }

    /// Returns `false` when `Input::Shutdown` requests a clean stop.
    fn handle(&mut self, input: Input) -> bool {
        match &input {
            Input::OpenLeader { session, .. } | Input::Propose { session, .. } => {
                self.scheduled.insert(session.clone());
                // A caller asking for the session is an event that could change a failed reopen,
                // or a failed snapshot that leaves its log stalled.
                if let Some(sg) = self.groups.get_mut(session) && sg.replica.is_stalled() { sg.snapshot_held = None; }
                if self.reopen_held.remove(session) { let session = session.clone(); self.reopen_fenced(&session); }
            }
            Input::Step { session, .. } |
            Input::ReportSnapshot { session, .. } | Input::Transfer { session, .. } | Input::SnapshotReady { session, .. } => {
                self.scheduled.insert(session.clone());
            }
            #[cfg(test)]
            Input::ForceCompact { session, .. } | Input::InjectRawProposal { session, .. } => { self.scheduled.insert(session.clone()); }
            _ => {}
        }
        match input {
            Input::Wake(session) => {
                if let Some(sg) = self.groups.get(&session) {
                    sg.wakeup.pending.store(false, Ordering::Release);
                    self.scheduled.insert(session);
                }
            }
            Input::Names(change, reply) => { let _ = reply.send(self.change_names(change)); }
            Input::Shutdown => return false,
            Input::SetStore(store) => {
                if self.store.as_ref().is_some_and(|old| std::ptr::eq(old.id(), store.id())) { return true; }
                self.store = Some(store.clone());
                self.shared.lock().store = Some(store);
                let sessions: Vec<_> = self.groups.keys().cloned().collect();
                for session in sessions {
                    self.reopen_attempts.remove(&session);
                    self.reopen_fenced(&session);
                }
                for sg in self.groups.values_mut() { sg.snapshot_held = None; }
                self.release_reopens();
            }
            Input::RegisterMembers(members) => {
                let mut names = Vec::with_capacity(members.len());
                let mut features_changed = false;
                for (name, v2, executor) in members {
                    if name.is_empty() || name == self.me { continue; }
                    let id = node_id(&name);
                    if v2 {
                        features_changed |= self.v2_peers.insert(id);
                    } else if self.v2_peers.remove(&id) {
                        features_changed = true;
                        self.wire_joiner.forget_prefix(&format!("{id}:"));
                        *self.wire_generation.entry(id).or_default() += 1;
                        self.snapshot_sends.retain(|send| send.to_id != id || send.total == 1);
                    }
                    features_changed |= self.executors.insert(id, executor) != Some(executor);
                    names.push(name);
                }
                if features_changed { self.scheduled.extend(self.groups.keys().cloned()); }
                self.handle(Input::RegisterPeers(names));
            }
            Input::RegisterPeers(names) => {
                let mut arrivals = Vec::new();
                for name in names {
                    if name.is_empty() || name == self.me { continue; }
                    let id = node_id(&name);
                    let new = self.announced.insert(id);
                    let returned = self.unreachable_peers.remove(&id);
                    if new {
                        self.terminal_pending.extend(self.terminal.iter().filter(|(_, group)| !group.members.contains(&id)).map(|(session, _)| session.clone()));
                    }
                    if new || returned { arrivals.push(id); self.peers.insert(id, name); }
                }
                if !arrivals.is_empty() {
                    self.release_reopens();
                    for (session, sg) in &mut self.groups {
                        if let Some(w)=&mut sg.process_watch && arrivals.contains(&w.leader) {w.answered=None; self.scheduled.insert(session.clone());}
                        if let Some(r)=&mut sg.process_round && arrivals.iter().any(|id|r.asked.contains(id)) {r.retry=Some(self.liveness.now_ns()); self.scheduled.insert(session.clone());}
                        if sg.replica.is_leader() && sg.replica.peers().any(|p|arrivals.contains(&p)) { let _=sg.replica.beat(); }
                        let conf = sg.replica.configuration();
                        if sg.replica.is_leader() || conf.voters.as_slice() == [self.my_id] || sg.replica.machine().closed_at_index.is_some() {
                            sg.wanted_learners.extend(arrivals.iter().copied().filter(|id| !conf.learners.contains(id) && !conf.voters.contains(id)));
                            self.scheduled.insert(session.clone());
                        }
                    }
                }
            }
            Input::SetPeerV2(peer_name, supported) => {
                let peer = node_id(&peer_name);
                let changed = if supported { self.v2_peers.insert(peer) } else { self.v2_peers.remove(&peer) };
                if changed {
                    if !supported {
                        self.wire_joiner.forget_prefix(&format!("{peer}:"));
                        *self.wire_generation.entry(peer).or_default() += 1;
                        self.snapshot_sends.retain(|send| send.to_id != peer || send.total == 1);
                    }
                    self.scheduled.extend(self.groups.keys().cloned());
                }
            }
            Input::SetPeerExecutor(peer_name, supported) => {
                let peer = node_id(&peer_name);
                if peer != self.my_id && self.executors.insert(peer, supported) != Some(supported) { self.scheduled.extend(self.groups.keys().cloned()); }
            }
            Input::PeerUnreachable(peer_name) => {
                let pid = node_id(&peer_name);
                if !self.unreachable_peers.insert(pid) { return true; }
                self.wire_joiner.forget_prefix(&format!("{pid}:"));
                *self.wire_generation.entry(pid).or_default() += 1;
                self.v2_peers.remove(&pid);
                self.snapshot_sends.retain(|send| send.to_id != pid);
                self.snapshots_lost_to(pid);
                for id in self.by_peer.get(&pid).into_iter().flatten() {
                    if let Some(sg)=self.groups.get_mut(id) {
                        let _=sg.replica.report_unreachable(pid);
                        self.scheduled.insert(id.clone());
                    }
                }
                self.scheduled.extend(self.transfers.iter().filter(|(_, (target, _))| *target == pid).map(|(id, _)| id.clone()));
                self.scheduled.extend(self.groups.iter().filter(|(_, sg)| sg.waits()).map(|(id, _)| id.clone()));
            }
            Input::ReportSnapshot { session, peer_name, arrived } => {
                let pid = node_id(&peer_name);
                if !arrived {
                    self.snapshot_sends.retain(|send| send.session != session || send.to_id != pid);
                    self.snapshot_acks.remove(&(session.clone(), pid));
                }
                if let Some(sg) = self.groups.get_mut(&session)
                    && let Err(ReplicaError::Fenced(_)) = sg.replica.report_snapshot(pid, arrived)
                {
                    self.reopen_fenced(&session);
                }
            }
            Input::OpenLeader { policy, session, epoch, encoded, bytes, reply } => {
                if let Err(e) = self.ensure_leader_group(&session, epoch, policy, encoded, bytes, reply) {
                    tracing::warn!(%session, error = %format!("{e:#}"), "open_leader failed");
                }
            }
            Input::Propose { session, epoch, mut encoded, bytes, reply } => {
                if encoded.is_empty() {
                    release_admission(&self.admission, bytes);
                    if let Some(r) = reply {
                        let _ = r.send(Ok(()));
                    }
                    return true;
                }
                // Never silently create a brand-new singleton Raft group in `Propose`: only
                // `OpenLeader` (`start_leader`/`open_leader`) creates a leader group, so a retired
                // or foreign session can never split-brain into an unintended singleton leader.
                let my_id = self.my_id;
                let err = if let Some(sg) = self.groups.get_mut(&session) {
                    let current_epoch = sg.replica.machine().epoch;
                    let authorized = !sg.local_disk_slow && sg.replica.machine().published() && sg.replica.is_leader() && sg.replica.machine().owner_term == sg.replica.term() && sg.pending_claim.is_none() && sg.replica.machine().leader_name == self.me;
                    if epoch != current_epoch {
                        release_admission(&self.admission, bytes);
                        crate::metrics::convo_raft().dropped_proposals.inc();
                        tracing::warn!(%session, node = %self.me, sent_epoch = epoch, current_epoch, "refused conversation Raft proposal under superseded ownership epoch");
                        Some(anyhow::anyhow!("stale ownership epoch {epoch} for session {session} (current={current_epoch})"))
                    } else if !authorized {
                        release_admission(&self.admission, bytes);
                        crate::metrics::convo_raft().dropped_proposals.inc();
                        tracing::warn!(%session, node = %self.me, leader = sg.replica.leader(), entries = encoded.len(), "refused conversation Raft proposal on non-leader replica");
                        Some(anyhow::anyhow!("node {} is not leader/voter for session {session}", self.me))
                    } else {
                        // Attach a commit-confirmation tag when `reply` is present (`propose_sync`)
                        // so the caller waits until true WAL + Raft quorum commit + apply rather
                        // than returning on in-memory queue admission!
                        if let Some(r) = reply {
                            let tag = CommitTag::new(my_id, sg.replica.term());
                            if let Some(last) = encoded.last_mut() {
                                last.commit_tag = Some(tag);
                            }
                            sg.commit_waiters.insert(tag, r);
                        }
                        // Keep `bytes` reserved in `self.admission` across `sg.pending_proposals`
                        // until each item actually leaves the queue (proposed to Raft or dropped).
                        let mut accounted = 0usize;
                        let total_items = encoded.len();
                        for (i, mut item) in encoded.drain(..).enumerate() {
                            let share = if i + 1 == total_items { bytes.saturating_sub(accounted) } else { item.whole.len().min(bytes.saturating_sub(accounted)) };
                            accounted = accounted.saturating_add(share);
                            item.reserved_bytes = share;
                            sg.pending_proposals.push_back(item);
                        }
                        self.dirty_sessions.insert(session.clone());
                        return true;
                    }
                } else {
                    release_admission(&self.admission, bytes);
                    crate::metrics::convo_raft().dropped_proposals.inc();
                    Some(anyhow::anyhow!("session Raft group {session} does not exist"))
                };
                if let (Some(r), Some(e)) = (reply, err) {
                    let _ = r.send(Err(e));
                }
            }
            Input::Transfer { session, target_node, epoch, digest, reply } => {
                let target_id = node_id(&target_node);
                let local = target_node == self.me;
                let eligible = if local { self.local_exec } else {
                    self.peers.get(&target_id) == Some(&target_node) && !self.unreachable_peers.contains(&target_id) && self.executors.get(&target_id) == Some(&true)
                };
                let accepted = if eligible && let Some(sg) = self.groups.get_mut(&session) {
                    let machine = sg.replica.machine();
                    let manifest = digest.as_ref().is_none_or(|digest| machine.native_move.as_ref().is_some_and(|m| m.epoch == epoch && m.target == target_node && m.digest == *digest));
                    let owns = !sg.local_disk_slow && manifest && machine.epoch == epoch && sg.replica.is_leader() && machine.published() && machine.owner_term == sg.replica.term() && machine.leader_name == self.me;
                    if owns && local && digest.is_some() {
                        // An in-place recovery retains its elected node but replaces its execution epoch.
                        sg.propose_claim(&session, &self.me, self.my_id,None)
                    } else {
                        if owns && !local {
                            if let Some(prior) = self.transfers.get(&session) {
                                let _ = reply.send(prior == &(target_id,digest));
                                return true;
                            }
                            if digest.is_none() && !machine.native_move.as_ref().is_some_and(|m|m.leadership_only && m.epoch==epoch && m.target==target_node) {
                                let intent = MoveIntent { leadership_only: true, cancel: false, ready: false, epoch, target: target_node, digest: String::new() };
                                if machine.native_move.is_some() || sg.ownership_command(&session, &self.me, false, Some(intent)).is_err() {
                                    let _ = reply.send(false);
                                    return true;
                                }
                            }
                            sg.wanted_learners.insert(target_id);
                            self.transfers.insert(session, (target_id, digest));
                        }
                        owns
                    }
                } else { false };
                let _ = reply.send(accepted);
            }
            Input::Step { from_node, session, bytes } => {
                {
                    let mut admission = self.admission.lock();
                    admission.incoming = admission.incoming.saturating_sub(bytes.capacity().saturating_add(session.len()).saturating_add(from_node.len()).saturating_add(size_of::<Input>()));
                }
                // Reassemble chunked wire frames (`KRC1`, e.g. multi-MiB full-history MsgSnapshot).
                let is_krc1 = bytes.len() >= WIRE_CHUNK_HDR && bytes.get(..4) == Some(WIRE_CHUNK_MAGIC.as_slice());
                if is_krc1 {
                    let peer = node_id(&from_node);
                    if !self.v2_peers.contains(&peer) { return true; }
                    let id = bytes.get(4..12).and_then(|s| s.try_into().ok()).map(u64::from_be_bytes).unwrap_or(0);
                    let part = bytes.get(12..16).and_then(|s| s.try_into().ok()).map(u32::from_be_bytes).unwrap_or(0);
                    let parts = bytes.get(16..20).and_then(|s| s.try_into().ok()).map(u32::from_be_bytes).unwrap_or(0);
                    let key = format!("{peer}:{session}");
                    if let Some(complete) = self.wire_joiner.add_frame(&key, id, part, parts, bytes, WIRE_CHUNK_HDR) {
                        let charge = complete.encoded_bytes().saturating_mul(2);
                        if self.snapshots_in_flight >= self.max_snapshot_workers || (self.snapshots_in_flight != 0 && self.snapshot_bytes.saturating_add(charge) > crate::memory::room::share()) { return true; }
                        let peer_version = self.wire_generation.get(&peer).copied().unwrap_or(0);
                        let tx = self.tx.clone();
                        match thread::Builder::new().name("kiln-convo-read-wire".into()).spawn(move || {
                            let message = complete.finish().and_then(|bytes| Message::decode(&bytes).ok()).map(Box::new);
                            let _ = tx.send(Input::IncomingSnapshot { from: from_node, session, peer_version, charge, message });
                        }) {
                            Ok(_) => { self.snapshots_in_flight += 1; self.snapshot_bytes = self.snapshot_bytes.saturating_add(charge); }
                            Err(e) => tracing::warn!(%e, "starting incoming snapshot reader failed"),
                        }
                    }
                } else if let Ok(message) = Message::decode(&bytes) {
                    self.step_message(from_node, session, message);
                }
            }
            Input::IncomingSnapshot { from, session, peer_version, charge, message } => {
                self.snapshots_in_flight = self.snapshots_in_flight.saturating_sub(1);
                self.snapshot_bytes = self.snapshot_bytes.saturating_sub(charge);
                let peer = node_id(&from);
                if self.wire_generation.get(&peer).copied().unwrap_or(0) == peer_version && self.v2_peers.contains(&peer) && let Some(msg) = message {
                    self.scheduled.insert(session.clone());
                    self.step_message(from, session, *msg);
                }
            }
            Input::SnapshotStaged { session, peer, generation, charge, result } => {
                self.snapshots_in_flight = self.snapshots_in_flight.saturating_sub(1);
                self.snapshot_bytes = self.snapshot_bytes.saturating_sub(charge);
                if let Some(sg) = self.groups.get_mut(&session) && generation.ptr_eq(&Arc::downgrade(&sg.wakeup)) {
                    match result {
                        Ok(send) if sg.replica.is_leader() && sg.replica.term() == send.term && !self.unreachable_peers.contains(&peer) && (send.total == 1 || self.v2_peers.contains(&peer)) => {
                            self.snapshot_sends.retain(|prior| prior.session != session || prior.to_id != peer);
                            self.snapshot_sends.push_back(send);
                        }
                        Ok(_) => { let _ = sg.replica.report_snapshot(peer, false); }
                        Err(error) => {
                            tracing::warn!(%session, %peer, %error, "staging snapshot transfer failed");
                            let _ = sg.replica.report_snapshot(peer, false);
                            self.scheduled.insert(session);
                        }
                    }
                }
            }
            Input::SnapshotPinned { session, generation } => {
                if let Some(sg) = self.groups.get_mut(&session) && generation.ptr_eq(&Arc::downgrade(&sg.wakeup)) {
                    sg.snapshot_pinning = false;
                    self.scheduled.insert(session);
                }
            }
            Input::SnapshotReady { session, generation, at, conf, keep, charge, result } => {
                self.on_snapshot_ready(&session, generation, at, conf, keep, charge, result);
            }
            #[cfg(test)]
            Input::ProcessRefusal(session,peer,tx) => {
                let refused=self.groups.get(&session).and_then(|sg|sg.process_round.as_ref()).and_then(|r|r.answers.get(&peer)).is_some_and(|v| !v.lost);
                let _=tx.send(refused);
            }
            #[cfg(test)]
            Input::NextTimer(reply) => {
                let _=reply.send(self.groups.values().flat_map(|g|[g.replica.deadline(),g.process_round.as_ref().and_then(|r|r.retry)]).flatten().min());
            }
            #[cfg(test)]
            Input::DriveDue => {
                self.on_deadline();
                let now = self.now_ns();
                self.scheduled.extend(self.groups.iter().filter(|(_,g)|
                    [g.replica.deadline(),g.process_round.as_ref().and_then(|r|r.retry)].into_iter().flatten().any(|at|at<=now)).map(|(id,_)|id.clone()));
            }
            Input::OutputReady => self.scheduled.extend(std::mem::take(&mut self.waiting_output).into_keys()),
            Input::NodeEvidence => {
                let mut events=std::mem::take(&mut self.evidence_buf);
                self.evidence.lock().take(&mut events);
                for ev in events.drain(..) { self.node_evidence(ev); }
                self.evidence_buf=events;
            }
            Input::FlushWait(reply) => {
                self.flush_waiters.push(reply);
            }
            #[cfg(test)]
            Input::ForceCompact { session, keep, reply } => {
                if let Some(sg) = self.groups.get_mut(&session) {
                    sg.compact_waiters.push(reply);
                    self.spawn_snapshot_build(&session, keep, true);
                } else {
                    let _ = reply.send(Err(anyhow::anyhow!("missing group {session}")));
                }
            }
            #[cfg(test)]
            Input::InjectRawProposal { session, raw, reply } => {
                let res = match self.groups.get_mut(&session) {
                    Some(sg) => {
                        sg.pending_proposals.push_back(PendingProposal::raw(raw));
                        self.dirty_sessions.insert(session.clone());
                        Ok(())
                    }
                    None => Err(anyhow::anyhow!("missing group {session}")),
                };
                let _ = reply.send(res);
            }
            #[cfg(test)]
            Input::FailNextPersist { session, reply } => {
                let res = match self.groups.get_mut(&session) {
                    Some(sg) => {
                        sg.replica.machine().fail_next_persist.store(true, Ordering::Release);
                        Ok(())
                    }
                    None => Err(anyhow::anyhow!("missing group {session}")),
                };
                let _ = reply.send(res);
            }
            #[cfg(test)]
            Input::SnapshotState { session, reply } => {
                let st = self.groups.get(&session).map_or((0, false, 0), |sg| {
                    (
                        sg.snapshot_failures,
                        !sg.snapshot_free(),
                        sg.replica.machine().in_flight_parts.len(),
                    )
                });
                let _ = reply.send(st);
            }
        }
        true
    }

    /// A fenced member has no running replica to subscribe to its leader's detector.
    /// Ask its saved voters for a Raft probe on the existing bounded repair retry.
    fn probe_saved_voters(&self, session: &str, slot: SessionSlot) -> anyhow::Result<()> {
        #[derive(Deserialize)]
        struct SavedMembership {
            session: String,
            conf: Conf,
        }
        let checkpoint: SavedMembership =
            serde_json::from_reader(std::io::BufReader::new(std::fs::File::open(self.state_path(slot.gid))?))?;
        anyhow::ensure!(checkpoint.session == session, "mismatched repair checkpoint");
        let conf = ConfState::from(&checkpoint.conf);
        for &id in conf
            .voters
            .iter()
            .chain(conf.voters_outgoing.iter().filter(|p| !conf.voters.contains(p)))
        {
            if id == self.my_id || self.unreachable_peers.contains(&id) {
                continue;
            }
            if let Some(to) = self.peers.get(&id) {
                let mut probe = hyper_raft::proto::message(id, MessageType::MsgHeartbeatResponse);
                probe.from = self.my_id;
                probe.context = b"KGP1".to_vec();
                let _ = self.out_tx.try_send(Outgoing {
                    session: session.to_owned(),
                    to_id: id,
                    to_name: to.clone(),
                    bytes: probe.encode_to_vec(),
                    is_snapshot: false,
                    snapshot_last: false,
                });
            }
        }
        Ok(())
    }

    fn receive_repair(&mut self, session: &str, from: &str, msg: &Message) -> anyhow::Result<()> {
        let &slot = self.session_slots.get(session).context("missing durable repair slot")?;
        if !(matches!(msg.msg_type, MessageType::MsgAppend | MessageType::MsgHeartbeat | MessageType::MsgSnapshot)
            || msg.msg_type==MessageType::MsgHeartbeatResponse && msg.context==b"KGP2") { return Ok(()); }
        let requesting = msg.msg_type != MessageType::MsgSnapshot;
        if requesting && self.repair_outstanding(session, msg.from, msg.term) { return Ok(()); }
        let state = self.state_path(slot.gid);
        let saved: SavedConvo = serde_json::from_slice(&std::fs::read(&state)?)?;
        let conf = ConfState::from(&saved.conf);
        anyhow::ensure!(saved.session == session && (conf.voters.contains(&msg.from) || conf.voters_outgoing.contains(&msg.from)), "peer is not a saved session voter");
        let shard = self.shards.get(&slot.shard).context("repair WAL is unavailable")?;
        let mut log = GroupStore::claim(&shard.log, slot.gid).map_err(|e| anyhow::anyhow!("claiming repair WAL: {e:?}"))?;
        let view = log.view()?;
        anyhow::ensure!(msg.term >= view.hard_state.term, "stale repair sender term");
        if requesting {
            let mut request = hyper_raft::proto::message(msg.from, MessageType::MsgAppendResponse);
            request.from = self.my_id;
            request.term = msg.term;
            request.index = view.hard_state.commit;
            request.reject = true;
            request.reject_hint = view.last;
            request.request_snapshot = view.last.max(view.hard_state.held_index).max(saved.index).max(1);
            request.log_term = log.term(view.last).map_err(|e| anyhow::anyhow!("reading repair term: {e:?}"))?;
            if self.out_tx.try_send(Outgoing { session: session.to_owned(), to_id: msg.from, to_name: from.to_owned(), bytes: request.encode_to_vec(), is_snapshot: false, snapshot_last: false }).is_ok() {
                self.repair_requests.insert(session.to_owned(), (msg.from, msg.term, Instant::now()));
            }
        } else {
            let journal = state.with_extension("peer-repair");
            let mut machine = ConvoMachine::open(session.to_owned(), self.names.clone(), self.my_id,None,self.store.clone(),state)?;
            machine.repair_from(&mut log, msg, self.my_id, &journal, true)?;
            drop(log);
            self.reopen_attempts.remove(session); self.reopen_held.remove(session);
            self.reopen_fenced(session);
        }
        Ok(())
    }

    /// A minority's rejected, unpublished bootstrap can adopt the winner. Both electorates
    /// must match, the winner's image must include its majority certificate, and its WAL
    /// frontier must cover every promise already made by this replica.
    fn reconcile_genesis(&mut self, session: &str, from: &str, msg: &Message) -> anyhow::Result<()> {
        let outstanding = self.repair_outstanding(session, msg.from, msg.term);
        let sg = self.groups.get_mut(session).context("missing provisional group")?;
        let machine = sg.replica.machine();
        if machine.founding_index.is_none() || machine.genesis.is_none()
            || !msg.context.starts_with(b"KGS1") || msg.term < sg.replica.term() { return Ok(()); }
        match msg.msg_type {
            MessageType::MsgAppend | MessageType::MsgHeartbeat | MessageType::MsgHeartbeatResponse => {
                if outstanding { return Ok(()); }
                let log = sg.replica.log();
                let view = log.view()?;
                let mut request = hyper_raft::proto::message(msg.from, MessageType::MsgAppendResponse);
                request.from = self.my_id;
                request.term = msg.term;
                request.index = msg.index.max(msg.commit).max(view.last);
                request.reject = true;
                request.reject_hint = view.last;
                request.request_snapshot = view.last.max(view.hard_state.held_index).max(machine.applied.index).max(1);
                request.log_term = log.term(view.last).map_err(|e| anyhow::anyhow!("reading provisional WAL: {e:?}"))?;
                if self.out_tx.try_send(Outgoing { session: session.to_owned(), to_id: msg.from, to_name: from.to_owned(), bytes: request.encode_to_vec(), is_snapshot: false, snapshot_last: false }).is_ok() {
                    self.repair_requests.insert(session.to_owned(), (msg.from, msg.term, Instant::now()));
                }
            }
            MessageType::MsgSnapshot => {
                machine.check_repair(sg.replica.log(), msg, self.my_id)?;
                save_atomic(&machine.state_file.with_extension("peer-repair"), &msg.encode_to_vec())?;
                for waiter in sg.open_waiters.drain(..) {
                    let _ = waiter.reply.send(Err(anyhow::anyhow!("session {session} was founded by {from}; read its committed session status")));
                }
                for (_, tx) in sg.commit_waiters.drain() { let _ = tx.send(Err(anyhow::anyhow!("session id belongs to another genesis"))); }
                release_admission(&self.admission, sg.pending_proposals.drain(..).map(|p| p.reserved_bytes).sum());
                release_admission(&self.admission, sg.credits.drain().map(|(_, charge)| charge).sum());
                sg.founding_epoch = None;
                self.reopen_attempts.remove(session); self.reopen_held.remove(session);
                self.reopen_fenced(session);
            }
            _ => {}
        }
        Ok(())
    }

    fn step_message(&mut self, from_node: String, session: String, mut msg: Message) {
        // Authenticate Raft sender id against the transport-verified `from_node` identity
        // so a peer cannot spoof another runner's Raft member id or leader identity.
        let expected_id = node_id(&from_node);
        if from_node.is_empty() || expected_id == self.my_id || msg.to != self.my_id || msg.from == 0 || msg.from != expected_id {
            tracing::warn!(%session, from = %from_node, msg_from = msg.from, expected = expected_id, "refused convo Raft message whose sender id disagreed with authenticated peer");
            return;
        }
        if msg.context.starts_with(super::custody::MARKER) {
            use super::custody::{Wire,Watch,voters};
            if msg.msg_type!=MessageType::MsgHeartbeatResponse {return;}
            let Some(sg)=self.groups.get_mut(&session) else {return};
            let Some(frame)=Wire::read(&msg.context) else {return};
            let q=match frame {Wire::Check(q)|Wire::Answer{query:q,..}=>q};
            if msg.term!=q.term || !sg.matches_process_query(q) || !voters(sg.replica.configuration()).any(|v|v==expected_id) {return;}
            match frame {
                Wire::Check(_) if sg.replica.leader()==expected_id && sg.membership.shared.with(|l|l.observed_run(expected_id)).flatten()==Some(q.leader_run) && voters(sg.replica.configuration()).any(|v|v==self.my_id) => {
                    if let Some(w)=&mut sg.process_watch && w.leader==expected_id && w.query.term==q.term {
                        if w.query.nonce>q.nonce {return;}
                        if w.query==q {w.answered=None;} else {**w=Watch{leader:expected_id,query:q,answered:None,serial:0};}
                    } else { sg.process_watch=Some(Box::new(Watch{leader:expected_id,query:q,answered:None,serial:0})); }
                }
                Wire::Answer{witness_run,serial,lost,..} if sg.replica.is_leader() && expected_id!=q.owner => {
                    if sg.membership.shared.with(|l|l.observed_run(expected_id)).flatten()!=Some(witness_run) {return;}
                    let conf=sg.replica.configuration();
                    let Some(r)=&mut sg.process_round else {return};
                    if r.query!=q || !r.asked.contains(&expected_id) || r.voters!=conf.voters || r.outgoing!=conf.voters_outgoing {return;}
                    r.answer(conf,expected_id,witness_run,serial,lost,sg.membership.shared.now_ns());
                }
                _=>return,
            }
            self.scheduled.insert(session);
            return;
        }
        if msg.msg_type==MessageType::MsgHeartbeatResponse && msg.term==0 && msg.context==b"KGI1" {
            if let Some(sg)=self.groups.get(&session) && sg.replica.is_leader() && sg.replica.machine().published()
                && sg.replica.peers().any(|p|p==expected_id) && let Some(g)=sg.replica.machine().genesis.as_ref() {
                let mut m=hyper_raft::proto::message(expected_id,MessageType::MsgHeartbeatResponse);
                m.from=self.my_id; m.term=sg.replica.term(); m.context=b"KGS1".to_vec(); m.context.extend_from_slice(&g.digest);
                let _=self.out_tx.try_send(Outgoing{session,to_id:expected_id,to_name:from_node,bytes:m.encode_to_vec(),is_snapshot:false,snapshot_last:false});
            }
            return;
        }
        if msg.msg_type==MessageType::MsgHeartbeatResponse && msg.term==0 && msg.context==b"KGP1" {
            if let Some(sg)=self.groups.get(&session) && sg.replica.is_leader() && sg.replica.peers().any(|p|p==expected_id) {
                // An application repair answer only names the actual term. Neither
                // discovery frame enters a Raft core or generates group-wide traffic.
                let mut answer=hyper_raft::proto::message(expected_id,MessageType::MsgHeartbeatResponse);
                answer.from=self.my_id; answer.term=sg.replica.term(); answer.context=b"KGP2".to_vec();
                let _=self.out_tx.try_send(Outgoing{session,to_id:expected_id,to_name:from_node,bytes:answer.encode_to_vec(),is_snapshot:false,snapshot_last:false});
            }
            return;
        }
        if msg.msg_type==MessageType::MsgHeartbeatResponse && msg.context==b"KGP2" {
            if self.recovering.contains_key(&session) {let _=self.receive_repair(&session,&from_node,&msg);}
            return;
        }
        if self.executors.get(&expected_id) == Some(&false)
            && matches!(msg.msg_type, MessageType::MsgRequestVote | MessageType::MsgRequestPreVote) { return; }
        if msg.msg_type == MessageType::MsgTimeoutNow && self.groups.get(&session).is_none_or(|g|
            g.replica.configuration().voters.as_slice() != [self.my_id] && !g.membership.timed()) { return; }
        if !self.local_exec && matches!(msg.msg_type, MessageType::MsgTimeoutNow | MessageType::MsgRequestVoteResponse | MessageType::MsgRequestPreVoteResponse) { return; }
        if msg.msg_type == MessageType::MsgAppendResponse && !msg.held && !msg.reject {
            let key = (session.clone(), expected_id);
            if self.snapshot_acks.get(&key).is_some_and(|(idx, term, _)| msg.term == *term && msg.index >= *idx) { self.snapshot_acks.remove(&key); }
        }
        if let std::collections::btree_map::Entry::Vacant(entry) = self.peers.entry(expected_id) {
            entry.insert(from_node.clone());
            self.shared.lock().enrolled.insert(expected_id);
        }
        if self.unreachable_peers.contains(&expected_id) {
            self.handle(Input::RegisterPeers(vec![from_node.clone()]));
        }
        if !self.groups.contains_key(&session) {
            if self.recovering.contains_key(&session) {
                if let Err(e) = self.receive_repair(&session, &from_node, &msg) { tracing::debug!(%session, %e, "session replica still needs a verified repair image"); }
                return;
            }
            if !self.peer_may_initiate(&session, &from_node, &msg) {
                tracing::warn!(%session, from = %from_node, msg_type = ?msg.msg_type, "refused unsolicited convo Raft group creation violating local custody");
                return;
            }
            let conf = ConfState { voters: vec![expected_id], learners: vec![self.my_id], ..ConfState::default() };
            if let Err(e) = self.create_group(&session, conf, from_node.clone(), None) {
                tracing::error!(%session, from = %from_node, error = %format!("{e:#}"), "opening follower WAL group failed");
                return;
            }
        }
        if msg.context.len() == 36 && msg.context.starts_with(b"KGS1") && self.groups.get(&session).is_some_and(|sg| sg.replica.machine().genesis.as_ref().is_some_and(|g| msg.context.get(4..) != Some(g.digest.as_slice()))) {
            if let Err(e) = self.reconcile_genesis(&session, &from_node, &msg) { tracing::debug!(%session, %e, "waiting for a certified session genesis"); }
            return;
        }
        let mut fenced = false;
        let mut stalled = false;
        if let Some(sg) = self.groups.get_mut(&session) {
            let machine = sg.replica.machine();
            if msg.context.len() == 36 && msg.context.starts_with(b"KGX2") {
                if let Some(g) = &machine.genesis && machine.founding_index.is_some() && sg.replica.is_leader()
                    && msg.msg_type == MessageType::MsgHeartbeatResponse && msg.term == sg.replica.term()
                    && msg.context.get(4..) == Some(g.digest.as_slice()) && g.members.binary_search(&expected_id).is_ok()
                    && sg.rejected_witnesses.insert(expected_id) && sg.conflicting_names() {
                        self.dirty_sessions.insert(session.clone());
                        // The refusal can have been sent before its winning name was
                        // sealed. Ask the voters which refused for the published root.
                        for &peer in &sg.rejected_witnesses {
                            if let Some(name)=self.peers.get(&peer) {
                                let mut q=hyper_raft::proto::message(peer,MessageType::MsgHeartbeatResponse);
                                q.from=self.my_id; q.context=b"KGI1".to_vec();
                                let _=self.out_tx.try_send(Outgoing{session:session.clone(),to_id:peer,to_name:name.clone(),bytes:q.encode_to_vec(),is_snapshot:false,snapshot_last:false});
                            }
                        }
                }
                return;
            }
            // A one-time voter can positively refuse an impossible candidate. Keep this
            // response outside Raft progress: it reports only the separate namespace vote.
            if msg.context.len() == 36 && msg.context.starts_with(b"KGR1") && matches!(msg.msg_type, MessageType::MsgHeartbeat | MessageType::MsgAppend)
                && machine.name_vote.is_some_and(|ours| msg.context.get(4..) != Some(ours.as_slice())) {
                // The winner's original publication beat can be lost while a rival
                // is partitioned. Answer its actual request with the sealed digest as
                // well as the refusal. Only a certified snapshot can replace its WAL.
                if let Some(g) = machine.genesis.as_ref()
                    && machine.published() && sg.replica.is_leader() && sg.replica.peers().any(|p| p == expected_id)
                {
                    let mut published = hyper_raft::proto::message(msg.from, MessageType::MsgHeartbeatResponse);
                    published.from = self.my_id;
                    published.term = sg.replica.term();
                    published.context = Vec::with_capacity(4 + g.digest.len());
                    published.context.extend_from_slice(b"KGS1");
                    published.context.extend_from_slice(&g.digest);
                    let _ = self.out_tx.try_send(Outgoing { session:session.clone(), to_id:expected_id, to_name:from_node.clone(), bytes:published.encode_to_vec(), is_snapshot:false, snapshot_last:false });
                }
                let mut refusal = hyper_raft::proto::message(msg.from, MessageType::MsgHeartbeatResponse);
                refusal.from = self.my_id;
                refusal.term = msg.term;
                refusal.context = msg.context;
                if let Some(marker) = refusal.context.get_mut(..4) { marker.copy_from_slice(b"KGX2"); }
                let _ = self.out_tx.try_send(Outgoing { session: session.clone(), to_id: expected_id, to_name: from_node.clone(), bytes: refusal.encode_to_vec(), is_snapshot: false, snapshot_last: false });
                return;
            }
            let mut attested = machine.genesis.is_none();
            // Request and durable witness use different markers. An older peer echoing a
            // heartbeat challenge cannot accidentally vote for a genesis it has never applied.
            if msg.context.len() == 36 && (msg.context.starts_with(b"KGR1") || msg.context.starts_with(b"KGW2") || msg.context.starts_with(b"KGA2") || msg.context.starts_with(b"KGW1") || msg.context.starts_with(b"KGS1")) {
                if sg.replica.machine().founded && sg.replica.machine().genesis.is_none() { return; }
                if let Some(g) = &sg.replica.machine().genesis {
                    if msg.context.get(4..) != Some(g.digest.as_slice()) { return; }
                    let witness = msg.context.starts_with(b"KGW2") || (!sg.replica.machine().names.read().required && msg.context.starts_with(b"KGW1"));
                    attested = witness || msg.context.starts_with(b"KGA2");
                    if witness && !msg.reject && msg.commit >= g.index && msg.term == sg.replica.term()
                        && matches!(msg.msg_type, MessageType::MsgAppendResponse | MessageType::MsgHeartbeatResponse)
                        && sg.replica.machine().founding_index.is_some() && g.members.binary_search(&expected_id).is_ok() {
                        sg.genesis_witnesses.insert(expected_id);
                    }
                }
                msg.context.clear();
            }
            // A new leader may never have compacted. Its storage returns
            // SnapshotTemporarilyUnavailable until an exact image is prepared.
            let conf = sg.replica.configuration();
            if msg.msg_type == MessageType::MsgAppendResponse && msg.reject && msg.request_snapshot > 0
                && sg.replica.is_leader() && msg.term == sg.replica.term() && sg.replica.applied().index >= msg.request_snapshot
                && (conf.learners.contains(&expected_id) || conf.voters.contains(&expected_id))
                && sg.replica.machine().recovery_image.as_ref().is_none_or(|img| img.index < msg.request_snapshot)
                && sg.snapshot_free() {
                stalled = true;
            }
            let before_role = (sg.replica.term(), sg.replica.leader(), sg.replica.is_leader());
            // The core's commit field is the follower's WAL-durable frontier. Even if its
            // separate match index is only a held promise, the durable commit is exact.
            let peer_commit = (attested && sg.replica.is_leader() && sg.replica.term() == msg.term && !msg.reject
                && matches!(msg.msg_type, MessageType::MsgAppendResponse | MessageType::MsgHeartbeatResponse)
                && (sg.replica.configuration().learners.contains(&expected_id) || sg.replica.configuration().voters.contains(&expected_id)))
                .then_some((msg.term, msg.commit));
            match sg.replica.step(msg) {
                Ok(()) => if let Some(frontier) = peer_commit {
                    sg.peer_commit = sg.peer_commit.max(frontier);
                    if sg.replica.machine().founding_barrier == FoundingBarrier::VotingFleet && sg.replica.configuration().voters.contains(&expected_id) {
                        let commit = sg.founding_voter_commits.entry(expected_id).or_default();
                        *commit = (*commit).max(frontier);
                    }
                },
                Err(ReplicaError::Fenced(cause)) => {
                    tracing::error!(%session, from = %from_node, %cause, "convo Raft replica fenced on step; reopening");
                    fenced = true;
                }
                Err(ReplicaError::Stalled) => {
                    crate::metrics::convo_raft().stalled_events.inc();
                    stalled = true;
                }
                Err(e) => {
                    tracing::debug!(%session, from = %from_node, "convo Raft step refused: {e:?}");
                }
            }
            if before_role != (sg.replica.term(), sg.replica.leader(), sg.replica.is_leader()) {
                self.dirty_sessions.insert(session.clone());
            }
            // Do NOT overwrite `machine().leader_name` here before `drive_all`: if this
            // step just elected THIS node via `MsgRequestVoteResp`, `drive_all` needs the
            // previous `leader_name` intact to detect failover takeover and emit `Claimed`!
        }
        if fenced {
            self.reopen_fenced(&session);
        } else if stalled {
            self.spawn_snapshot_build(&session, 0, false);
        }
    }

    fn ensure_leader_group(&mut self, session: &str, epoch: u64, policy: Policy, encoded: Vec<PendingProposal>, bytes: usize, reply: Option<Sender<anyhow::Result<GroupStatus>>>) -> anyhow::Result<()> {
        let waiter = if let Some(reply) = reply {
            if let Err(reason) = reserve_admission(&self.admission, size_of::<OpenWaiter>()) {
                release_admission(&self.admission, bytes);
                let _ = reply.send(Err(anyhow::anyhow!(reason)));
                anyhow::bail!(reason);
            }
            Some(OpenWaiter { reply, admission: self.admission.clone() })
        } else { None };
        let existing = self.groups.contains_key(session);
        let result = (|| -> anyhow::Result<()> {
            anyhow::ensure!(self.local_exec, "this node has no session host");
            if !self.groups.get(session).is_some_and(|sg| sg.replica.machine().founded || sg.founding_epoch.is_some()) {
                // The namespace can freeze while encoding a large opening operation.
                // Reject its obsolete basis before creating a slot or applying any rows.
                #[derive(Deserialize)]
                struct Basis { genesis_members: Vec<u64>, #[serde(default)] genesis_charter: Option<[u8;32]> }
                let first = encoded.first().context("founding proposal has no data")?;
                let n = self.names.read();
                if n.required {
                    let basis: Basis = serde_json::from_slice(&first.whole)?;
                    let historic = n.active.as_ref().is_some_and(|c| c.reserved.contains(session));
                    if let Some(c) = &n.active && !historic {
                        anyhow::ensure!(basis.genesis_charter == Some(c.digest) && basis.genesis_members == c.voters, "the name charter changed while creating {session}; retry");
                    } else {
                        let registrar = n.pin.registrar.as_ref().context("authenticated name registrar unavailable")?;
                        anyhow::ensure!(basis.genesis_charter.is_none() && basis.genesis_members.as_slice() == [node_id(registrar)], "the name authority changed while creating {session}; retry against its original WAL");
                        anyhow::ensure!(!n.pin.frozen || existing, "session name is frozen; recover its original WAL and selected node");
                        if !n.pin.frozen && registrar == &self.me {
                            anyhow::ensure!(n.registrar_can_grant(session), "session name awaits verified pre-upgrade history; reconnect its original holder or recover its WAL");
                        }
                        if n.pin.frozen && registrar == &self.me {
                            let pending = n.pin.pending.get(session).context("historic session has no unfinished founding reservation; restore its original WAL")?;
                            anyhow::ensure!(pending.selected == self.me && pending.digest.is_none_or(|d| d == <[u8;32]>::from(sha2::Sha256::digest(&first.whole))), "recover the exact original founding operation for {session}");
                        }
                    }
                }
            }
            if !existing {
                if encoded.iter().any(PendingProposal::needs_chunking) {
                    anyhow::ensure!(self.peers.keys().all(|id| *id == self.my_id || self.unreachable_peers.contains(id) || self.v2_peers.contains(id)), "a connected fleet member cannot receive the founding history for {session}");
                }
                anyhow::ensure!(!self.terminal.contains_key(session), "session {session} is already closed");
                anyhow::ensure!(!self.store.as_ref().context("conversation store unavailable")?.raft_projection_frontier(session)?.is_some_and(|(index,_)| index != 0), "session {session} has prior committed history but no active WAL slot");
                let conf = ConfState { voters: vec![self.my_id], ..ConfState::default() };
                let mut connected = self.peers.keys().filter(|id| **id != self.my_id);
                let eligible = connected.clone().filter(|id| self.executors.get(id) == Some(&true)).count();
                let barrier = if policy.auto_failover && eligible.saturating_add(1) >= policy.voters() {
                    FoundingBarrier::VotingFleet
                } else if connected.next().is_some() { FoundingBarrier::RemoteCopy } else { FoundingBarrier::Local };
                self.create_group(session, conf, self.me.clone(), Some(barrier))?;
            }
            let sg = self.groups.get(session).context("missing session group after open")?;
            anyhow::ensure!(!sg.local_disk_slow, "selected node cannot confirm custody while its WAL is stalled");
            if existing {
                anyhow::ensure!(!sg.conflicting_names(), "conflicting starts for session {session} have incompatible durable name votes; inspect the session status or use another id");
                anyhow::ensure!(sg.replica.machine().leader_name == self.me && (
                    (!sg.replica.machine().founded && sg.replica.configuration().voters.as_slice() == [self.my_id] && sg.founding_epoch.is_none_or(|claimed| claimed == epoch)) ||
                    (sg.replica.is_leader() && sg.replica.machine().owner_term == sg.replica.term() && sg.replica.machine().founded && sg.replica.machine().epoch == epoch && sg.pending_claim.is_none())
                ), "session {session} already records selected node {}; inspect its committed session status", sg.replica.machine().leader_name);
            }
            Ok(())
        })();
        if let Err(err) = result {
            release_admission(&self.admission, bytes);
            if let Some(waiter) = waiter { let _ = waiter.reply.send(Err(anyhow::anyhow!("{err:#}"))); }
            return Err(err);
        }
        let sg = self.groups.get_mut(session).context("session lost during open")?;
        if sg.replica.machine().founded || sg.founding_epoch.is_some() {
            release_admission(&self.admission, bytes);
        } else {
            sg.founding_epoch = Some(epoch);
            sg.pending_proposals.extend(encoded);
        }
        if let Some(waiter) = waiter { sg.open_waiters.push(waiter); }
        Ok(())
    }

    fn create_group(&mut self, session: &str, conf: ConfState, default_leader: String, founding: Option<FoundingBarrier>) -> anyhow::Result<()> {
        anyhow::ensure!(!self.archive_path(session).try_exists()?, "session {session} is already closed or its archive needs recovery");
        anyhow::ensure!(!self.session_slots.contains_key(session), "session {session} already has a durable WAL slot awaiting recovery");
        let (slot, group_store) = self.claim_new_group(session)?;
        let depth = group_store.depth();
        let state_file = self.state_path(slot.gid);
        let mut machine = ConvoMachine::open(session.to_owned(), self.names.clone(), self.my_id,Some((default_leader,conf)),self.store.clone(),state_file)?;
        if let Some(barrier) = founding && machine.founding_barrier != barrier {
            machine.founding_barrier = barrier;
            machine.save_checkpoint().map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        let mut replica = Replica::open(&raft_settings(self.my_id, depth)?, group_store, machine, Bytes::new(u64::try_from(crate::memory::room::share()).unwrap_or(u64::MAX)))
            .map_err(|e| anyhow::anyhow!("opening WAL Raft group for {session}: {e:?}"))?;
        let compacted = replica.log().bounds().map_or(0, |(base, _)| base.index);
        let wanted_learners = if founding.is_some() {
            anyhow::ensure!(self.local_exec && replica.configuration().voters.as_slice()==[self.my_id], "selected founder must be its group's executable sole voter");
            replica.campaign()?;
            self.peers.keys().copied().filter(|id| *id != self.my_id).collect()
        } else {
            BTreeSet::new()
        };
        self.session_slots.insert(session.to_owned(), slot);
        if let Err(e) = self.save_sessions_index() {
            self.session_slots.remove(session);
            return Err(e);
        }
        self.dirty_sessions.insert(session.to_owned());
        self.scheduled.insert(session.to_owned());
        let wakeup = Wakeup::new(session, &self.tx);
        self.groups.insert(
            session.to_owned(),
            SessionGroup {
                membership: super::liveness::Membership::new(self.liveness.clone(), slot.gid, None, self.local_exec),
                indexed_membership:0, local_disk_slow: false, process_round:None, process_watch:None, reaffirm_term: None, abandon_term: None, held_owner: None, lost_owner: None, deadline: None,
                rejected_witnesses: BTreeSet::new(), genesis_witnesses: BTreeSet::new(), sealing_term: None, founding_epoch: None,
                publication_term:None, blocked_proof: 0, sent_proof: None, peer_commit: (0, 0), founding_voter_commits: BTreeMap::new(), applied_waiters: VecDeque::new(),
                bookkeeping_term: 0,
                v2_suffix: false,
                waker: Waker::from(Arc::clone(&wakeup)),
                wakeup,
                replica,
                wanted_learners,
                changing: None,
                pending_proposals: VecDeque::new(),
                credits: HashMap::new(),
                commit_waiters: HashMap::new(),
                pending_claim: None,
                open_waiters: Vec::new(),
                snapshot_building: false,
                snapshot_pinning: false,
                snapshot_failures: 0,
                snapshot_held: None,
                was_leader: false,
                reopened_at_index: None,
                #[cfg(test)]
                compact_waiters: Vec::new(),
                needs_restart_campaign: false,
                compacted,
                compacting: false,
                writes_out: 0,
            },
        );
        self.link_group(session,true)?;
        Ok(())
    }

    fn next_deadline(&self) -> Option<Instant> {
        if !self.terminal_pending.is_empty() || !self.scheduled.is_empty() { return Some(Instant::now()); }
        self.deadlines.first().map(|d|d.0).into_iter()
            .chain(self.terminal_retry.values().map(|(_,at)|*at))
            .chain(self.snapshot_acks.values().map(|(_,_,due)|*due)).min()
    }

    fn on_deadline(&mut self) {
        let now = Instant::now();
        while self.deadlines.first().is_some_and(|d| d.0 <= now) {
            let Some((at,id)) = self.deadlines.pop_first() else { break; };
            if let Some(sg) = self.groups.get_mut(&id) && sg.deadline == Some(at) { sg.deadline = None; self.scheduled.insert(id); }
        }
        self.terminal_pending.extend(self.terminal_retry.iter().filter(|(_,(_,at))| now >= *at).map(|(s,_)|s.clone()));
        self.snapshot_acks.retain(|(session,peer),(_,term,due)| {
            let Some(sg) = self.groups.get_mut(session) else { return false; };
            if sg.replica.term() != *term || !sg.replica.is_leader() { return false; }
            if now < *due { return true; }
            let _ = sg.replica.report_snapshot(*peer,false);
            self.scheduled.insert(session.clone());
            false
        });
    }

    fn next_group_wake(&mut self, session: &str) {
        let Some(sg) = self.groups.get_mut(session) else { return };
        let now = Instant::now();
        let clock = self.liveness.now_ns();
        let due = (!sg.snapshot_pinning && sg.replica.fenced().is_none()).then(||sg.replica.deadline()).flatten().and_then(|at| now.checked_add(Duration::from_nanos(at.saturating_sub(clock))))
            .into_iter()
            .chain(sg.process_round.as_ref().filter(|_|!sg.snapshot_pinning && sg.replica.fenced().is_none()).and_then(|r|r.retry).and_then(|t|now.checked_add(Duration::from_nanos(t.saturating_sub(clock)))))
            .chain(sg.changing.and_then(|(_,_,at)|at.checked_add(sg.membership.election()?)).filter(|t|*t > now)).min();
        if due == sg.deadline { return; }
        if let Some(old) = sg.deadline.take() { self.deadlines.remove(&(old,session.to_owned())); }
        if let Some(at) = due { self.deadlines.insert((at,session.to_owned())); sg.deadline=Some(at); }
    }

    fn link_group(&mut self, session:&str, force:bool) -> anyhow::Result<()> {
        let Some(sg) = self.groups.get_mut(session) else { return Ok(()); };
        sg.membership.reconcile(&mut sg.replica,force)?;
        if force {
            sg.local_disk_slow = self.liveness.with(|l|l.own_disk_slow()) == Some(true);
            let owner = node_id(&sg.replica.machine().leader_name);
            if owner != self.my_id {
                let process_lost = self.liveness.with(|l|l.stopped(owner) || l.trust(owner)==Some(hyper_timing::Trust::Suspected))==Some(true);
                sg.lost_owner=process_lost.then_some(owner);
                if process_lost { sg.held_owner=None; }
                else if sg.replica.disk_is_slow(owner) { sg.held_owner=Some(owner); }
            }
        }
        if sg.indexed_membership != sg.membership.revision {
            sg.indexed_membership = sg.membership.revision;
            self.by_peer.retain(|peer, ids| {
                if !sg.membership.peers.iter().any(|p|p.1==*peer) { ids.remove(session); }
                !ids.is_empty()
            });
            for &(_,id) in &sg.membership.peers { self.by_peer.entry(id).or_default().insert(session.to_owned()); }
        }
        Ok(())
    }

    fn unlink_group(&mut self,session:&str) {
        if let Some(sg) = self.groups.get(session) {
            if let Some(at) = sg.deadline { self.deadlines.remove(&(at,session.to_owned())); }
            for &(_,peer) in &sg.membership.peers {
                if let Some(set) = self.by_peer.get_mut(&peer) { set.remove(session); if set.is_empty() { self.by_peer.remove(&peer); } }
            }
        }
    }

    /// Every snapshot outstanding to `peer`, failed: it became unreachable, or was suspected, stopped or restarted, so
    /// no acknowledgement of what was sent it is waited for. Its group sends one again as its core asks.
    fn snapshots_lost_to(&mut self, peer: u64) {
        let lost: Vec<String> = self.snapshot_acks.keys().filter(|(_, to)| *to == peer).map(|(session, _)| session.clone()).collect();
        for session in lost {
            self.snapshot_acks.remove(&(session.clone(), peer));
            if let Some(sg) = self.groups.get_mut(&session) {
                if let Err(ReplicaError::Fenced(_)) = sg.replica.report_snapshot(peer, false) {
                    self.reopen_fenced(&session);
                    continue;
                }
                self.scheduled.insert(session);
            }
        }
    }

    /// The retransmission timeout of the path to `peer`: RFC 6298's `SRTT + max(G, 4·RTTVAR)` over the round trips the
    /// node's liveness stream measured to it (`ExchangeRtt::tail_ns`, RFC 9002 §5.3's estimate), plus the node's mean
    /// flush, as an answer leaves only once what it answers is durable; [`RTO_UNMEASURED`] before any is measured.
    fn path_rto(&self, peer: u64) -> Duration {
        self.liveness
            .with(|l| {
                let granularity = u64::try_from(l.granularity()?.as_nanos()).unwrap_or(u64::MAX);
                let tail = l.round_trip(peer)?.tail_ns(granularity)?;
                Some(Duration::from_nanos(tail).saturating_add(l.flush_mean().unwrap_or(Duration::ZERO)))
            })
            .flatten()
            .unwrap_or(RTO_UNMEASURED)
    }

    /// A repair request to `peer` in `term`, sent at `sent`, still outstanding: not answered (reopening clears it), its
    /// term current (the key holds it), its peer not suspected, stopped or restarted since (`node_evidence` clears
    /// those), and younger than the path's measured retransmission timeout, past which one lost on a live peer is asked
    /// again on that peer's next message. No timer: it's judged as the next message comes.
    fn repair_outstanding(&self, session: &str, peer: u64, term: u64) -> bool {
        self.repair_requests.get(session).is_some_and(|(asked, at, sent)| *asked == peer && *at == term && sent.elapsed() < self.path_rto(peer))
    }

    fn node_evidence(&mut self, event: crate::liveness::Event) {
        use crate::liveness::Event;
        // A peer suspected, stopped or restarted: what was asked of it, or was on its way from it, is no longer waited
        // for, so the next message asks again and a partial message of its run goes.
        if let Event::Change(c) = &event
            && matches!(c, hyper_liveness::Change::Suspected(_) | hyper_liveness::Change::Stopped { .. } | hyper_liveness::Change::Restarted { .. })
        {
            let peer = c.peer();
            self.repair_requests.retain(|_, (asked, _, _)| *asked != peer);
            if !matches!(c, hyper_liveness::Change::Suspected(_)) {
                self.wire_joiner.forget_prefix(&format!("{peer}:"));
            }
            self.snapshots_lost_to(peer);
        }
        if let Event::Change(c) = &event {
            let peer = c.peer();
            self.scheduled.extend(self.transfers.iter().filter(|(_, (target, _))| *target == peer).map(|(id, _)| id.clone()));
            self.scheduled.extend(self.groups.iter().filter(|(_, sg)| sg.waits()).map(|(id, _)| id.clone()));
            if matches!(c, hyper_liveness::Change::Trusted { .. } | hyper_liveness::Change::Restarted { .. }) { self.release_reopens(); }
        }
        if matches!(event, Event::LocalDisk) {
            for sg in self.groups.values_mut() { sg.snapshot_held = None; }
            self.release_reopens();
        }
        if let Event::Timing(peer) = event {
            for id in self.by_peer.get(&peer).into_iter().flatten() {
                let Some(sg) = self.groups.get_mut(id) else { continue };
                let previous = sg.membership.election();
                if let Err(e) = sg.membership.measure(&mut sg.replica) {
                    tracing::debug!(session=%id,%e,"waiting for measured session timing");
                }
                // The first measurement can release an unarmed election or retry.
                // Recalibration leaves ordinary idle groups parked.
                let next = sg.membership.election();
                if previous != next && (previous.is_none() || next.is_none() || sg.changing.is_some()) {
                    self.scheduled.insert(id.clone());
                }
            }
            return;
        }
        let peer = match event {
            Event::Change(c) => Some(c.peer()),
            _ => None,
        };
        let ids: Vec<_> = match peer {
            Some(p) => self.by_peer.get(&p).into_iter().flatten().cloned().collect(),
            None => self.groups.keys().cloned().collect(),
        };
        for id in ids {
            if matches!(event, Event::Ready)
                && let Err(e) = self.link_group(&id, true)
            {
                tracing::warn!(session=%id,%e,"unable to attach session to node liveness");
                continue;
            }
            let Some(sg) = self.groups.get_mut(&id) else { continue };
            let owner = node_id(&sg.replica.machine().leader_name);
            match event {
                Event::Change(c) => {
                    if matches!(c,hyper_liveness::Change::Stopped{..}|hyper_liveness::Change::Restarted{..}|hyper_liveness::Change::Suspected(_))
                        && sg.process_round.as_ref().is_some_and(|r|r.answers.contains_key(&c.peer())) {sg.process_round=None;}
                    if let Err(e) = sg.membership.belief(&mut sg.replica, c) {
                        tracing::warn!(session=%id,%e,"session refused detector verdict");
                    }
                    if c.peer() == owner {
                        match c {
                            hyper_liveness::Change::Suspected(_)
                            | hyper_liveness::Change::Stopped { .. }
                            | hyper_liveness::Change::Restarted { .. } => {
                                sg.lost_owner = Some(owner);
                                sg.held_owner = None;
                            }
                            hyper_liveness::Change::Trusted { .. } => sg.lost_owner = None,
                            hyper_liveness::Change::DiskSlow { .. } if sg.lost_owner != Some(owner) => sg.held_owner = Some(owner),
                            _ => {}
                        }
                    }
                }
                Event::Timing(_) => continue,
                Event::LocalDisk => {
                    sg.local_disk_slow = self.liveness.with(|l| l.own_disk_slow()) == Some(true);
                    if sg.local_disk_slow
                        && owner == self.my_id
                        && sg.replica.is_leader()
                        && sg.replica.machine().owner_term == sg.replica.term()
                    {
                        sg.held_owner = Some(owner);
                    }
                }
                Event::Ready => {}
            }
            if sg.held_owner == Some(owner) && owner == self.my_id && !sg.replica.is_leader() && !sg.has_process_quorum(self.my_id) {
                sg.held_owner = None;
            }
            self.dirty_sessions.insert(id.clone());
            self.scheduled.insert(id);
        }
    }

    fn now_ns(&self) -> u64 { self.liveness.now_ns() }

    fn drive_all(&mut self) {
        let now = self.now_ns();
        let mut snap_bundles = Vec::new();
        let mut failed_snapshots: Vec<(String, u64)> = Vec::new();
        let mut finished_transfers = Vec::new();
        let mut fenced_sessions = Vec::new();
        let mut cleared_backoff = Vec::new();
        let mut snapshot_requests: Vec<(String, u64)> = Vec::new();
        let mut failovers: Vec<FailoverEvent> = Vec::new();
        let mut output: Output<ApplyReceipt> = Output::default();
        let mut committed_tags: Vec<ApplyReceipt> = Vec::new();
        let mut next_wire_id = self.next_wire_id;
        #[cfg(test)]
        let mut chunks_sent = 0u64;

        let mut scheduled = std::mem::take(&mut self.scheduled);
        for session in &scheduled {
            let Some(sg) = self.groups.get_mut(session) else { continue };
            sg.blocked_proof = 0;
            // Pause this group's applies only until the read transaction has pinned its view.
            if sg.snapshot_pinning { continue; }
            // A fenced replica is reopened, or held after a failed reopen (`reopen_fenced`).
            if sg.replica.fenced().is_some() {
                fenced_sessions.push(session.clone());
                continue;
            }
            if sg.replica.machine_mut().record_name_vote().is_err() {
                fenced_sessions.push(session.clone());
                continue;
            }
            let before_applied = sg.replica.applied().index;
            let before_term = sg.replica.term();
            let before_leader = sg.replica.leader();
            let before_epoch = sg.replica.machine().epoch;
            let owned_before = sg.replica.machine().founded && sg.replica.machine().leader_name == self.me;
            let before_voters_len = sg.replica.configuration().voters.len();
            let before_learners_len = sg.replica.configuration().learners.len();

            committed_tags.clear();
            let mut hit_fenced = false;
            match drive_one(
                session,
                sg,
                now,
                &self.peers,
                &self.v2_peers,
                self.local_exec,
                &mut output,
                &mut committed_tags,
                &self.out_tx,
                &mut snap_bundles,
                &mut failed_snapshots,
                &mut next_wire_id,
            ) {
                Ok(_c) => {
                    #[cfg(test)]
                    {
                        chunks_sent = chunks_sent.saturating_add(_c);
                    }
                }
                Err(ReplicaError::Fenced(cause)) => {
                    tracing::error!(%session, %cause, "conversation Raft replica fenced during drive; scheduling reopen");
                    hit_fenced = true;
                }
                Err(ReplicaError::Stalled) => {
                    crate::metrics::convo_raft().stalled_events.inc();
                    if !sg.snapshot_building && sg.snapshot_free() {
                        snapshot_requests.push((session.clone(), 0));
                    }
                }
                Err(e) => {
                    tracing::warn!(%session, error = %e, "conversation Raft drive stopped");
                }
            }
            settle_committed_tags(session, sg, &mut committed_tags, &mut failovers, &self.admission);
            if hit_fenced {
                fenced_sessions.push(session.clone());
                continue;
            }
            // Clear reopen backoff once a reopened replica commits new entries cleanly.
            if sg.reopened_at_index.is_some_and(|idx| sg.replica.applied().index > idx) {
                sg.reopened_at_index = None;
                cleared_backoff.push(session.clone());
            }
            // Only campaign a reopened group AFTER committed WAL entries finish replaying so
            // any committed leadership transfer / conf change in `convo-raft-*.log` is honored.
            if sg.needs_restart_campaign && sg.writes_out == 0 {
                sg.needs_restart_campaign = false;
                if self.local_exec && !sg.replica.is_leader() && (sg.replica.configuration().voters.as_slice() == [self.my_id] || (sg.membership.timed() && sg.replica.machine().closed_at_index.is_some() && !sg.wanted_learners.is_empty() && sg.replica.configuration().voters.contains(&self.my_id))) {
                    let _ = sg.replica.campaign();
                    if let Err(ReplicaError::Fenced(_)) = drive_one(
                        session,
                        sg,
                        now,
                        &self.peers,
                        &self.v2_peers,
                        self.local_exec,
                        &mut output,
                        &mut committed_tags,
                        &self.out_tx,
                        &mut snap_bundles,
                        &mut failed_snapshots,
                        &mut next_wire_id,
                    ) {
                        fenced_sessions.push(session.clone());
                        continue;
                    }
                    settle_committed_tags(session, sg, &mut committed_tags, &mut failovers, &self.admission);
                }
            }
            // Automatic failover ownership takeover (commit-before-claim!) & stale-leader fencing:
            // - If this node just WON a Raft election for a live session previously led by another
            //   node, enqueue a tagged claim proposal for `new_epoch = epoch + 1` and record
            //   `sg.pending_claim`. DO NOT advance `machine.epoch` or emit `FailoverEvent::Claimed`
            //   until that claim entry actually commits through Raft quorum and applies!
            // - If this node previously led and has now stepped down (to another leader OR via
            //   `check_quorum` while partitioned), discard pending proposals, fail pending commit
            //   waiters, clear any uncommitted `pending_claim`, and emit `LostLeadership`.
            let lid = sg.replica.leader();
            let term = sg.replica.term();
            if sg.replica.machine().owner_term == term && sg.replica.machine().epoch != before_epoch { sg.held_owner = None; sg.lost_owner = None; }
            if sg.bookkeeping_term != term {
                sg.bookkeeping_term = term;
                sg.credits.retain(|tag, bytes| {
                    if tag.term == term { return true }
                    release_admission(&self.admission, *bytes);
                    false
                });
                sg.commit_waiters.retain(|tag, tx| {
                    if tag.term == term { return true; }
                    let _ = tx.send(Err(anyhow::anyhow!("leadership term changed before confirming {session}")));
                    false
                });
                sg.pending_proposals.retain(|item| {
                    if item.commit_tag.is_none_or(|tag| tag.term == term) { return true; }
                    release_admission(&self.admission, item.reserved_bytes);
                    false
                });
            }
            sg.answer_process_watch(session,&self.peers,&self.out_tx);
            if sg.process_round.as_ref().is_some_and(|r|sg.local_disk_slow || !sg.replica.is_leader() || !sg.matches_process_query(r.query) || sg.replica.machine().closed_at_index.is_some() || sg.replica.machine().owner_term==sg.replica.term() || sg.pending_claim.is_some()) {
                sg.process_round=None;
            }
            let mut leadership_output=false;
            if sg.replica.is_leader() {
                sg.was_leader = true;
                if sg.local_disk_slow { leadership_output|=sg.membership.yield_stalled_log(&mut sg.replica, |peer| self.executors.get(&peer) == Some(&true)).unwrap_or(false); }
                if !sg.local_disk_slow && sg.pending_claim.is_none() && sg.replica.machine().owner_term != sg.replica.term() && sg.replica.machine().founded && sg.replica.machine().closed_at_index.is_none() {
                    let machine = sg.replica.machine();
                    let owner = node_id(&machine.leader_name);
                    let requested = machine.native_move.as_ref().is_some_and(|m| !m.ready && !m.cancel && m.epoch == machine.epoch && m.target == self.me);
                    let disk = sg.replica.disk_is_slow(owner);
                    let remembered_loss = sg.lost_owner == Some(owner);
                    let automatic=machine.published() && machine.policy.unwrap_or_default().auto_failover;
                    let observed=sg.process_verdict(owner);
                    let lost=observed.is_some_and(|(_,lost)|lost);
                    let confirmed=if !requested && owner!=self.my_id && automatic && let Some((run,true))=observed {
                        sg.corroborated_loss(session,owner,run,&self.peers,&self.out_tx)
                    } else {sg.process_round=None;false};
                    if requested || confirmed {
                        sg.wanted_learners.extend(self.peers.keys().copied().filter(|id| *id != self.my_id));
                        let proof=sg.process_round.as_ref().filter(|_|confirmed).map(|r|r.certificate(self.my_id));
                        if sg.propose_claim(session, &self.me, self.my_id,proof) {
                            tracing::info!(%session, new_leader = %self.me, previous_owner = owner, term, requested, remembered_loss, ?observed, disk_slow = disk, "proposed transfer of session execution");
                        }
                    } else if owner == self.my_id {
                        // Once quorum loss or restart fenced the old provider, a fresh
                        // execution epoch is required even when the same node wins again.
                        if sg.held_owner != Some(owner) || lost || remembered_loss {
                            sg.propose_claim(session,&self.me,self.my_id,None);
                        } else if sg.reaffirm_term != Some(term) && sg.ownership_command(session,&self.me,true,None).is_ok() {
                            sg.reaffirm_term = Some(term);
                        }
                    } else if !lost {
                        sg.held_owner = Some(owner);
                        if !disk && self.executors.get(&owner)==Some(&true) && sg.membership.can_receive_lead(owner) && sg.replica.applied().term == term && sg.replica.caught_up(owner) && sg.replica.configuration().voters.contains(&owner) {
                            leadership_output=sg.replica.transfer(owner).is_ok();
                        }
                    }
                }
            } else {
                // Step down detection covers BOTH seeing a new leader (`lid != my_id`) AND losing
                // quorum while partitioned (`check_quorum` steps the isolated leader down to follower
                // with `lid == 0` so its harness is fenced immediately instead of accepting work!).
                if sg.was_leader && lid != self.my_id {
                    sg.was_leader = false;
                    sg.changing = None;
                    if !sg.replica.machine().founded { sg.founding_epoch = None; }
                    sg.pending_claim = None;
                    let new_leader = if lid != 0 {
                        self.peers.get(&lid).cloned().unwrap_or_else(|| sg.replica.machine().leader_name.clone())
                    } else {
                        String::new()
                    };
                    let new_epoch = sg.replica.machine().epoch;
                    if !sg.pending_proposals.is_empty() {
                        let dropped = u64::try_from(sg.pending_proposals.len()).unwrap_or(u64::MAX);
                        let released: usize = sg.pending_proposals.drain(..).map(|p| p.reserved_bytes).sum();
                        release_admission(&self.admission, released);
                        crate::metrics::convo_raft().dropped_proposals.add(dropped);
                    }
                    release_admission(&self.admission, sg.credits.drain().map(|(_, bytes)| bytes).sum());
                    for (_, tx) in sg.commit_waiters.drain() {
                        let _ = tx.send(Err(anyhow::anyhow!("lost Raft leadership before commit for {session}")));
                    }
                    tracing::info!(%session, %new_leader, new_epoch, term = sg.replica.term(), "stepped down from conversation Raft leadership; fencing stale local ownership");
                    failovers.push(FailoverEvent::LostLeadership {
                        session: session.clone(),
                        new_epoch,
                        new_leader,
                        term: sg.replica.term(),
                    });
                }
            }
            if sg.replica.is_leader() {
                let mut mutated=leadership_output;
                let term = sg.replica.term();
                let all_members_v2 = {
                    let conf = sg.replica.configuration();
                    conf.voters
                        .iter()
                        .chain(conf.learners.iter()).chain(&conf.voters_outgoing).chain(&conf.learners_next)
                        .all(|id| *id == self.my_id || self.v2_peers.contains(id))
                };
                // Handle every hyper-durable ReplicaError variant per `docs/durable.md`:
                // `Refused`, `Stalled`, `Budget`, and `Marked` are transient (keep rows queued!);
                // `Stalled` immediately kicks off compaction to free WAL room; `Fenced` reopens
                // the replica from durable state without discarding queued proposals.
                while let Some(item) = sg.pending_proposals.front_mut() {
                    if item.reserved_bytes != 0 && item.commit_tag.is_none() {
                        item.commit_tag = Some(CommitTag::new(self.my_id, term));
                    }
                    if item.needs_chunking() && !all_members_v2 {
                        tracing::warn!(%session, "refusing WireEntry::Part oversized row because a member does not advertise convo-raft-v2");
                        if let Some(popped) = sg.pending_proposals.pop_front() {
                            release_admission(&self.admission, popped.reserved_bytes);
                            if let Some(tag) = popped.commit_tag
                                && let Some(tx) = sg.commit_waiters.remove(&tag)
                            {
                                let _ = tx.send(Err(anyhow::anyhow!("a peer in session {session} does not speak convo-raft-v2; refusing oversized WireEntry::Part")));
                            }
                        }
                        crate::metrics::convo_raft().dropped_proposals.inc();
                        continue;
                    }
                    let (frame, frame_tag) = match item.next_frame(term) {
                        Ok(frame) => frame,
                        Err(e) => {
                            tracing::error!(%session, error = %format!("{e:#}"), "encoding queued Raft proposal chunk failed");
                            if let Some(popped) = sg.pending_proposals.pop_front() {
                                release_admission(&self.admission, popped.reserved_bytes);
                                if let Some(tag) = popped.commit_tag
                                    && let Some(tx) = sg.commit_waiters.remove(&tag)
                                {
                                    let _ = tx.send(Err(e));
                                }
                            }
                            crate::metrics::convo_raft().dropped_proposals.inc();
                            continue;
                        }
                    };
                    let ctx = frame_tag.map_or_else(Vec::new, CommitTag::context);
                    match sg.replica.propose(ctx, frame) {
                        Ok(()) => {
                            sg.v2_suffix |= item.needs_chunking();
                            if item.advance() && let Some(popped) = sg.pending_proposals.pop_front()
                                && let Some(tag) = popped.commit_tag && popped.reserved_bytes != 0 {
                                sg.credits.insert(tag, popped.reserved_bytes);
                            }
                            mutated = true;
                        }
                        Err(ReplicaError::Refused(_) | ReplicaError::Budget(_) | ReplicaError::Marked) => break,
                        Err(ReplicaError::Stalled) => {
                            crate::metrics::convo_raft().stalled_events.inc();
                            if !sg.snapshot_building && sg.snapshot_free() {
                                snapshot_requests.push((session.clone(), 0));
                            }
                            break;
                        }
                        Err(ReplicaError::Fenced(cause)) => {
                            tracing::error!(%session, %cause, "conversation Raft replica fenced on propose; reopening with queued rows preserved");
                            hit_fenced = true;
                            break;
                        }
                    }
                }
                if hit_fenced {
                    fenced_sessions.push(session.clone());
                    continue;
                }
                let failover = sg.replica.machine().policy.unwrap_or_default().auto_failover && sg.genesis_quorum(self.my_id);
                let (proposed_change, learner_joined) = reconcile_learners(sg, &self.unreachable_peers, &self.v2_peers, &self.executors, failover);
                if proposed_change {
                    mutated = true;
                }
                if learner_joined {
                    let _ = sg.replica.beat();
                    mutated = true;
                }
                // CP-T3.2. The old leader gives up a hand-over once an event shows its target can't
                // take the lead, by the test that admitted the hand-over (`Input::Transfer`) and the
                // detector's verdict, and commits that it did, so every member and whoever waits on
                // it learns. While the target stays eligible, the attempt is made again.
                let machine = sg.replica.machine();
                let pending = machine.native_move.clone().filter(|m| !m.leadership_only && !m.cancel && !m.ready && m.epoch == machine.epoch && m.target != self.me);
                if let Some(m) = pending
                    && machine.leader_name == self.me && machine.owner_term == term && sg.abandon_term != Some(term) && sg.pending_claim.is_none() && !sg.local_disk_slow
                {
                    let target = node_id(&m.target);
                    let why = if self.peers.get(&target) != Some(&m.target) { Some("it left the fleet") }
                        else if self.unreachable_peers.contains(&target) { Some("it can't be reached") }
                        else if self.executors.get(&target) != Some(&true) { Some("it hosts no sessions") }
                        else if sg.membership.refuses_lead(target) { Some("the detector suspects it, saw it stop, or finds its disk slow") }
                        else { None };
                    if let Some(why) = why {
                        let given_up = MoveIntent { leadership_only: false, cancel: true, ready: false, epoch: m.epoch, target: m.target.clone(), digest: m.digest };
                        if sg.ownership_command(session, &self.me, false, Some(given_up)).is_ok() {
                            sg.abandon_term = Some(term);
                            mutated = true;
                            tracing::info!(%session, target = %m.target, why, "gave up the session's hand-over");
                        }
                    }
                }
                // The committed move survives a source restart or a timeout-now that
                // arrived before the destination could measure its first election.
                if !self.transfers.contains_key(session)
                    && let Some(intent) = sg.replica.machine().native_move.as_ref()
                    && intent.epoch == sg.replica.machine().epoch && !intent.cancel && !intent.ready
                    && intent.target != self.me && sg.replica.machine().leader_name == self.me
                {
                    let target = node_id(&intent.target);
                    if self.peers.get(&target) == Some(&intent.target) && self.executors.get(&target)==Some(&true) {
                        sg.wanted_learners.insert(target);
                        self.transfers.insert(session.clone(),(target, (!intent.leadership_only).then(||intent.digest.clone())));
                    }
                }
                let transferring = self.transfers.get(session).and_then(|(node, digest)| {
                    if digest.as_ref().is_some_and(|d| sg.replica.machine().native_move.as_ref().is_none_or(|m| &m.digest != d)) {
                        finished_transfers.push(session.clone());
                        None
                    } else if digest.is_some() || sg.replica.machine().native_move.as_ref().is_some_and(|m|
                        m.leadership_only && m.epoch == sg.replica.machine().epoch && node_id(&m.target) == *node) { Some(*node) } else { None }
                });
                if let Some(target_id) = transferring
                    && !self.unreachable_peers.contains(&target_id)
                    && self.executors.get(&target_id)==Some(&true)
                    && sg.membership.can_receive_lead(target_id)
                    && sg.replica.caught_up(target_id)
                {
                    if sg.replica.configuration().voters.contains(&target_id) {
                        // Raft suppresses duplicates while this transfer is in flight.
                        // Its measured deadline reopens the attempt on a lost packet.
                        if sg.replica.transfer(target_id).is_ok() { mutated = true; }
                    } else if sg.changing.is_none() && sg.replica.configuration().learners.contains(&target_id)
                        && sg.replica.change(Vec::new(), &conf_change(target_id, ConfChangeType::AddNode)).is_ok()
                    {
                        sg.changing = Some((target_id, true, Instant::now()));
                        mutated = true;
                    }
                }
                if mutated {
                    match drive_one(
                        session,
                        sg,
                        now,
                        &self.peers,
                        &self.v2_peers,
                        self.local_exec,
                        &mut output,
                        &mut committed_tags,
                        &self.out_tx,
                        &mut snap_bundles,
                        &mut failed_snapshots,
                        &mut next_wire_id,
                    ) {
                        Ok(_c) => {
                            #[cfg(test)]
                            {
                                chunks_sent = chunks_sent.saturating_add(_c);
                            }
                            settle_committed_tags(session, sg, &mut committed_tags, &mut failovers, &self.admission);
                        }
                        Err(ReplicaError::Fenced(_)) => {
                            fenced_sessions.push(session.clone());
                            continue;
                        }
                        Err(_) => {}
                    }
                }
            }
            if sg.compacting && sg.writes_out == 0 {
                if let Ok((base, last)) = sg.replica.log().bounds() {
                    sg.compacted = base.index;
                    if sg.credits.is_empty() && sg.replica.applied().index >= last && sg.replica.machine().chunked_through <= base.index {
                        sg.v2_suffix = false;
                    }
                }
                sg.compacting = false;
            }
            // Schedule off-thread snapshot + compaction when WAL retention reaches `sync::disk::due`
            // AND compacting with `keep` will genuinely advance `sg.compacted` (eliminating
            // no-progress snapshot loops on small/fresh sessions).
            if !sg.compacting && !sg.snapshot_building
                && sg.snapshot_free()
                && sg.replica.machine().in_flight_parts.is_empty()
                && crate::sync::disk::due(&sg.replica, sg.compacted)
                && sg.replica.applied().index > sg.compacted
                && (sg.replica.is_stalled() || (sg.pending_proposals.is_empty() && sg.writes_out == 0))
            {
                let keep = crate::sync::disk::keep(&sg.replica, sg.compacted);
                let can_advance = sg.replica.is_stalled() || sg.replica.applied().index.saturating_sub(keep) > sg.compacted;
                if can_advance {
                    snapshot_requests.push((session.clone(), keep));
                }
            }
            let lid = sg.replica.leader();
            if self.transfers.get(session).is_some_and(|(t,_)| lid == *t || !sg.replica.is_leader()) {
                finished_transfers.push(session.clone());
            }
            if owned_before && sg.replica.machine().leader_name != self.me && sg.replica.machine().epoch > before_epoch {
                failovers.push(FailoverEvent::LostLeadership {
                    session: session.clone(), new_epoch: sg.replica.machine().epoch,
                    new_leader: sg.replica.machine().leader_name.clone(), term: sg.replica.term(),
                });
            }
            // Mark session dirty for incremental `publish_shared()` only if observable status changed.
            let conf = sg.replica.configuration();
            if sg.replica.applied().index != before_applied
                || sg.replica.term() != before_term
                || lid != before_leader
                || sg.replica.machine().epoch != before_epoch
                || conf.voters.len() != before_voters_len
                || conf.learners.len() != before_learners_len
            {
                self.dirty_sessions.insert(session.clone());
            }
            if sg.replica.machine().closed_at_index.is_some() { self.closed.insert(session.clone()); } else { self.closed.remove(session); }
            // Answer any `open_leader` waiters once election + initial rows are durably flushed to WAL.
            let founding_barrier = sg.replica.machine().founding_barrier;
            let voters = sg.replica.configuration().voters.len();
            let formed = match founding_barrier {
                FoundingBarrier::VotingFleet => voters >= sg.replica.machine().policy.unwrap_or_default().voters(),
                FoundingBarrier::RemoteCopy => voters > 1,
                FoundingBarrier::Local => false,
            };
            // The final configuration is applied locally after leaving joint consensus. A
            // chartered incoming majority must confirm its durable commit through that point.
            // An offline third voter can catch up when it returns.
            let known = formed && sg.replica.configuration().voters_outgoing.is_empty() && (sg.replica.configuration_known() || {
                let through = sg.replica.applied().index;
                let term = sg.replica.term();
                sg.replica.is_leader() && sg.replica.machine().genesis.as_ref().is_some_and(|g| g.charter.is_some())
                    && sg.replica.configuration().voters.iter().filter(|member| {
                        if **member == self.my_id { sg.replica.durable_commit() >= through }
                        else { sg.founding_voter_commits.get(*member).is_some_and(|(t, index)| *t == term && *index >= through) }
                    }).count() > voters / 2
            });
            if known {
                sg.replica.machine_mut().founding_barrier = FoundingBarrier::Local;
                if sg.replica.machine_mut().save_checkpoint().is_err() {
                    sg.replica.machine_mut().founding_barrier = founding_barrier;
                    fenced_sessions.push(session.clone());
                    continue;
                }
                sg.founding_voter_commits.clear();
            }
            let replicated = sg.receipt_frontier();
            let machine = sg.replica.machine();
            if sg.genesis_quorum(self.my_id) && sg.replica.is_leader() && machine.owner_term == sg.replica.term() && machine.leader_name == self.me
                && machine.founding_index.is_some_and(|at| at <= replicated) && sg.sealing_term != Some(sg.replica.term()) {
                let mut witnesses = sg.genesis_witnesses.clone();
                if machine.genesis.as_ref().is_some_and(|g| machine.name_vote == Some(g.digest) && g.members.binary_search(&self.my_id).is_ok()) { witnesses.insert(self.my_id); }
                let publication = ConvoEntry { process_loss:None, owner_run:None, reaffirm: false, seal_voters: machine.genesis.as_ref().filter(|g| g.charter.is_some()).map(|g| g.members.clone()).unwrap_or_default(), genesis_charter: machine.genesis.as_ref().and_then(|g| g.charter), genesis_members: witnesses.into_iter().collect(), seal: machine.genesis.as_ref().map(|g| g.digest), opening: Some(true), session: session.clone(), leader: self.me.clone(), epoch: machine.epoch, rows: Vec::new(), policy: None, claim_from: None, native_move: None };
                if let Ok(encoded) = serde_json::to_vec(&publication) {
                    let mut proof = PendingProposal::from_entry(encoded);
                    proof.commit_tag = Some(CommitTag::new(self.my_id, sg.replica.term()));
                    sg.pending_proposals.push_front(proof);
                    sg.sealing_term = Some(sg.replica.term());
                    sg.waker.wake_by_ref();
                }
            }
            let published = machine.published();
            // Applies arrive in log-index order. A delayed cross-machine receipt retains its
            // original admission credit; a partition cannot build an uncharged waiter list.
            let blocked = unconfirmable(sg, self.my_id, &self.peers, &self.executors, &self.unreachable_peers);
            while let Some(&(tag,index,charge)) = sg.applied_waiters.front() {
                let current = sg.replica.is_leader() && tag.term == sg.replica.term();
                let success = published && current && index <= replicated;
                if !success && current && sg.commit_waiters.contains_key(&tag) && blocked.is_none() { break; }
                sg.applied_waiters.pop_front();
                release_admission(&self.admission, charge);
                if let Some(tx) = sg.commit_waiters.remove(&tag) {
                    self.commit_answers.push((tx, if success { Ok(()) } else { Err(anyhow::anyhow!("no durable replication confirmation for {session}{}", blocked.map(|why| format!(": {why}")).unwrap_or_default())) }));
                }
            }
            if !sg.open_waiters.is_empty() {
                let ready = !sg.local_disk_slow && published && replicated >= sg.replica.applied().index && sg.replica.is_leader() && sg.replica.machine().owner_term == sg.replica.term() && sg.replica.machine().founded && sg.replica.machine().leader_name == self.me && sg.pending_claim.is_none() && sg.pending_proposals.is_empty() && sg.writes_out == 0;
                let conflict = sg.conflicting_names();
                let taken = sg.replica.machine().founded && sg.replica.machine().leader_name != self.me;
                if ready || conflict || taken || blocked.is_some() {
                    let st = group_status(session, &self.me, self.my_id, sg, &self.peers);
                    for waiter in sg.open_waiters.drain(..) {
                        let historical = !published && machine.genesis.as_ref().is_some_and(|g| g.charter.is_none()) && machine.names.read().active.as_ref().is_some_and(|c| c.reserved.contains(session));
                        let answer = if ready { Ok(st.clone()) } else if conflict {
                            Err(anyhow::anyhow!("conflicting starts for session {session} have incompatible durable name votes; inspect its session status or use another id"))
                        } else if historical {
                            Err(anyhow::anyhow!("reserved session {session} has no completed proof; reconnect the original registrar and selected node, or restore their original WAL checkpoints"))
                        } else if taken {
                            Err(anyhow::anyhow!("{} took ownership of session {session} first", sg.replica.machine().leader_name))
                        } else { Err(anyhow::anyhow!("session ownership can't be confirmed: {} (leader {}, term {}, applied {}, epoch {}, founded {})", blocked.unwrap_or_default(), st.leader_id, st.term, st.applied, st.epoch, sg.replica.machine().founded)) };
                        self.open_answers.push((waiter.reply.clone(), answer));
                    }
                }
            }
        }
        for id in &scheduled {
            if let Some(weight) = self.groups.get(id).map(|g|g.blocked_proof).filter(|n|*n>0) { self.waiting_output.insert(id.clone(),weight); }
            if let Err(e)=self.link_group(id,false) { tracing::warn!(session=%id,%e,"session liveness membership deferred"); }
            self.next_group_wake(id);
        }
        scheduled.clear();
        if scheduled.capacity() > self.scheduled.capacity() {
            scheduled.extend(self.scheduled.drain());
            self.scheduled = scheduled;
        }
        self.next_wire_id = next_wire_id;
        #[cfg(test)]
        {
            self.wire_chunks_sent = self.wire_chunks_sent.saturating_add(chunks_sent);
        }
        for s in cleared_backoff {
            self.reopen_attempts.remove(&s);
        }
        for s in finished_transfers {
            self.transfers.remove(&s);
        }
        for (session, to_id) in failed_snapshots {
            if let Some(sg) = self.groups.get_mut(&session)
                && let Err(ReplicaError::Fenced(_)) = sg.replica.report_snapshot(to_id, false)
            {
                fenced_sessions.push(session);
            }
        }
        for s in fenced_sessions {
            self.reopen_fenced(&s);
        }
        for (s, keep) in snapshot_requests {
            self.spawn_snapshot_build(&s, keep, false);
        }
        for ev in failovers {
            let _ = self.failover_tx.try_send(ev);
        }
        for job in snap_bundles { self.stage_snapshot(job); }
        // Emit frames round-robin under the transport queue's byte credit. No whole-image fit
        // requirement. A durable append response from the receiver completes the Raft transfer.
        while let Some(mut send) = self.snapshot_sends.pop_front() {
            let Some(sg) = self.groups.get_mut(&send.session) else { continue };
            if !sg.replica.is_leader() || sg.replica.term() != send.term { continue; }
            if !send.has_credit(&self.out_tx) { self.snapshot_sends.push_front(send); break; }
            let frame = match send.frame() {
                Ok(frame) => frame,
                Err(e) => {
                    tracing::warn!(session = %send.session, peer = send.to_id, %e, "reading staged conversation snapshot failed");
                    let _ = sg.replica.report_snapshot(send.to_id, false);
                    continue;
                }
            };
            if let Err(rejected) = self.out_tx.try_send(frame) {
                if let SnapshotSource::Frame(bytes) = &mut send.source { *bytes = rejected.into_inner().bytes; }
                self.snapshot_sends.push_front(send);
                break;
            }
            send.part += 1;
            if send.part < send.total {
                self.snapshot_sends.push_back(send);
            } else {
                // Overdue past the path's measured retransmission timeout for each part and its acknowledgement: what
                // a snapshot lost on a live peer, with no event saying so, is judged by. Every other end is an event.
                let due = Instant::now() + self.path_rto(send.to_id).saturating_mul(send.total.saturating_add(1));
                self.snapshot_acks.insert((send.session, send.to_id), (send.index, send.term, due));
            }
        }
        if (!self.snapshot_sends.is_empty() || !self.waiting_output.is_empty()) && !self.out_tx.is_closed() {
            self.out_tx.room().on_release(&self.output_place,&self.output_waker);
            let held = self.out_tx.held();
            if self.snapshot_sends.front().is_some_and(|send|send.has_credit(&self.out_tx))
                || self.waiting_output.values().any(|n|held==0 || held.saturating_add(*n)<=self.out_tx.bound()) { self.output_waker.wake_by_ref(); }
        }
    }

    fn publish_shared(&mut self) {
        if self.dirty_sessions.is_empty() && self.removed_sessions.is_empty() {
            #[cfg(test)]
            {
                let mut sh = self.shared.lock();
                sh.shard_count = self.shards.len();
                sh.wire_chunks_sent = self.wire_chunks_sent;
            }
            return;
        }
        let mut sh = self.shared.lock();
        let mut custody_changed = !self.removed_sessions.is_empty();
        for s in self.removed_sessions.drain(..) {
            sh.note_custody(&s);
            if sh.statuses.remove(&s).is_some_and(|old| old.is_leader) {
                sh.leading_count = sh.leading_count.saturating_sub(1);
            }
        }
        for session in self.dirty_sessions.drain() {
            let status = self.groups.get(&session).map(|sg| group_status(&session, &self.me, self.my_id, sg, &self.peers)).or_else(|| {
                self.recovering.contains_key(&session).then(|| {
                    let last = sh.statuses.get(&session);
                    GroupStatus { execution_held: false, name_conflict: false, conflicting_start_request: None, recovering: true, founded: last.is_some_and(|s| s.founded), session: session.clone(), node: self.me.clone(), node_id: self.my_id,
                        native_move_ready: false, native_move_digest: None, abandoned_move: None, is_leader: false, owner: None, leader_id: 0, leader_name: None,
                        term: last.map_or(0, |s| s.term), epoch: last.map_or(0, |s| s.epoch), applied: last.map_or(0, |s| s.applied),
                        voters: Vec::new(), learners: Vec::new() }
                })
            });
            if let Some(st) = status {
                let new_lead = st.is_leader;
                let old = sh.statuses.get(&session);
                if st.founded && !old.is_some_and(|s|s.founded) && let Some(holds)=sh.start_holds.upgrade(){holds.founded(&session);}
                if old.is_none_or(|old| old.recovering != st.recovering || old.founded != st.founded || old.name_conflict != st.name_conflict || old.execution_held != st.execution_held || old.native_move_digest != st.native_move_digest || old.native_move_ready != st.native_move_ready || old.abandoned_move != st.abandoned_move || old.owner != st.owner || old.is_leader != st.is_leader || old.term != st.term || old.epoch != st.epoch || old.leader_id != st.leader_id) {
                    custody_changed = true;
                    sh.note_custody(&session);
                }
                let old_lead = sh.statuses.insert(session, st).is_some_and(|old| old.is_leader);
                if new_lead && !old_lead {
                    sh.leading_count = sh.leading_count.saturating_add(1);
                } else if !new_lead && old_lead {
                    sh.leading_count = sh.leading_count.saturating_sub(1);
                }
            } else if let Some(old) = sh.statuses.remove(&session) {
                sh.note_custody(&session);
                custody_changed = true;
                if old.is_leader { sh.leading_count = sh.leading_count.saturating_sub(1); }
            }
        }
        #[cfg(test)]
        {
            sh.shard_count = self.shards.len();
            sh.wire_chunks_sent = self.wire_chunks_sent;
        }
        if custody_changed { sh.changed.notify(); }
        let m = crate::metrics::convo_raft();
        m.groups.set(i64::try_from(sh.statuses.len()).unwrap_or(i64::MAX));
        m.leading.set(i64::try_from(sh.leading_count).unwrap_or(i64::MAX));
    }

    fn notify_flush_waiters(&mut self) {
        // Open must return after the public status can authorize the caller's next operation.
        for (tx, answer) in self.open_answers.drain(..) { let _ = tx.send(answer); }
        for (tx, answer) in self.commit_answers.drain(..) { let _ = tx.send(answer); }
        if self.flush_waiters.is_empty() {
            return;
        }
        let idle = self.snapshots_in_flight == 0 && self.terminal_pending.is_empty() && self.groups.values().all(|sg| !sg.needs_restart_campaign && !sg.snapshot_building && !sg.compacting && sg.writes_out == 0 && sg.pending_proposals.is_empty());
        let unhealthy = !self.recovering.is_empty() || self.groups.values().any(|sg| sg.replica.fenced().is_some());
        if idle || unhealthy {
            for tx in self.flush_waiters.drain(..) {
                let _ = tx.send(idle && !unhealthy);
            }
        }
    }
}

/// Build the authoritative Raft claim rows proposed by a node immediately after winning an
/// automatic failover election for `session`, advancing `epoch` and updating `info.target` to the
/// new owning node.
fn build_failover_claim_rows(session: &str, new_leader: &str, new_epoch: u64, prev: Option<&AppliedSession>) -> Vec<Row> {
    let target = if new_leader == "server" { "local".to_owned() } else { format!("agent:{new_leader}") };
    let info = prev.and_then(|s| s.info.as_deref()).map(|raw| {
        if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(raw) {
            if let Some(obj) = v.as_object_mut() {
                obj.insert("target".into(), serde_json::Value::String(target.clone()));
                obj.insert("epoch".into(), serde_json::Value::Number(new_epoch.into()));
            }
            serde_json::to_string(&v).unwrap_or_else(|_| raw.to_owned())
        } else {
            raw.to_owned()
        }
    });
    let state = prev.and_then(|s| s.state.as_deref()).map(|raw| {
        if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(raw) {
            if let Some(obj) = v.as_object_mut() {
                obj.insert("epoch".into(), serde_json::Value::Number(new_epoch.into()));
            }
            serde_json::to_string(&v).unwrap_or_else(|_| raw.to_owned())
        } else {
            raw.to_owned()
        }
    });
    vec![
        Row::Session {
            session: session.to_owned(),
            info,
            meta: prev.and_then(|s| s.meta.clone()),
            state,
            changes: prev.and_then(|s| s.changes.clone()),
            epoch: Some(new_epoch),
            open: true,
            at: crate::util::now_ms(),
        },
        Row::Machine { session: session.to_owned(), machine: new_leader.to_owned() },
    ]
}

/// Manages all per-session/conversation `hyper-raft` groups on this node, backed by mandatory
/// durable sharded `hyper-log` WAL files (`<dir>/convo-raft-NNN.log`).
pub struct Manager {
    live: Arc<crate::liveness::Shared>,
    names: Arc<parking_lot::RwLock<NameAdmission>>,
    executable: bool,
    me: String,
    my_id: u64,
    policy: Policy,
    tx: Sender<Input>,
    admission: Arc<Mutex<AdmissionState>>,
    thread: Mutex<Option<JoinHandle<()>>>,
    shared: Arc<Mutex<Shared>>,
    out_rx: crate::memory::queue::Receiver<Outgoing>,
    failover_rx: crate::memory::queue::Receiver<FailoverEvent>,
}

impl Drop for Manager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

static SERVER_GLOBAL: arc_swap::ArcSwapOption<Manager> = arc_swap::ArcSwapOption::const_empty();
static RUNNER_GLOBAL: arc_swap::ArcSwapOption<Manager> = arc_swap::ArcSwapOption::const_empty();

/// Publish the server's conversation Raft manager (separate from any in-process local runner).
pub fn set_server_global(manager: Option<Arc<Manager>>) {
    SERVER_GLOBAL.store(manager);
}

/// Active server conversation Raft manager, if started.
pub fn server_global() -> Option<Arc<Manager>> {
    SERVER_GLOBAL.load_full()
}

/// Publish this runner's conversation Raft manager (separate from any co-located server).
pub fn set_runner_global(manager: Option<Arc<Manager>>) {
    RUNNER_GLOBAL.store(manager);
}

/// Active runner conversation Raft manager, if started.
pub fn runner_global() -> Option<Arc<Manager>> {
    RUNNER_GLOBAL.load_full()
}

/// Backwards-compatible alias setting the runner global.
pub fn set_global(manager: Option<Arc<Manager>>) {
    set_runner_global(manager);
}

/// Active conversation Raft manager (runner global first, falling back to server global).
pub fn global() -> Option<Arc<Manager>> {
    runner_global().or_else(server_global)
}

impl Manager {
    /// Open (or create) a WAL-backed conversation Raft manager for node `me` in `dir`
    /// (`<dir>/convo-raft-NNN.log`), backed by its local conversation `store`, with default policy
    /// (learner replication on, automatic multi-voter failover off).
    pub fn open(me: impl Into<String>, dir: &Path, store: Option<Store>) -> anyhow::Result<Arc<Self>> {
        Self::open_with_policy(me, dir, store, Policy::default())
    }

    /// Open (or create) a WAL-backed conversation Raft manager with an explicit replication and
    /// failover [`Policy`].
    pub fn open_with_policy(me: impl Into<String>, dir: &Path, store: Option<Store>, policy: Policy) -> anyhow::Result<Arc<Self>> {
        Self::open_with_options(me, dir, store, SHARD_GROUPS, policy, true, !cfg!(test))
    }

    pub fn open_for_host(me: impl Into<String>, dir: &Path, store: Option<Store>, policy: Policy, executable: bool) -> anyhow::Result<Arc<Self>> {
        Self::open_with_options(me, dir, store, SHARD_GROUPS, policy, executable, true)
    }

    #[cfg(test)]
    fn open_with_shard_groups(me: impl Into<String>, dir: &Path, store: Option<Store>, shard_groups: usize) -> anyhow::Result<Arc<Self>> {
        Self::open_with_options(me, dir, store, shard_groups, Policy::default(), true, false)
    }

    fn open_with_options(me: impl Into<String>, dir: &Path, store: Option<Store>, shard_groups: usize, policy: Policy, executable: bool, require_names: bool) -> anyhow::Result<Arc<Self>> {
        anyhow::ensure!(store.is_some(), "conversation Raft requires an open conversation store");
        let me = me.into();
        let my_id = node_id(&me);
        let dir = dir.to_path_buf();
        let (out_tx, out_rx) = crate::memory::queue::bounded(&crate::memory::owners::CHANNELS, crate::memory::room::share());
        crate::memory::room::server().follow_input(out_tx.room());
        let (failover_tx, failover_rx) = crate::memory::queue::unbounded(&crate::memory::owners::CHANNELS);
        let admission = Arc::new(Mutex::new(AdmissionState::default()));
        let (tx, rx) = mpsc::channel::<Input>();
        let names = Arc::new(parking_lot::RwLock::new(NameAdmission { required: require_names, ..Default::default() }));
        let owner_names = names.clone();
        #[cfg(not(test))]
        let live = crate::liveness::Shared::get(my_id);
        #[cfg(test)]
        let live = if require_names {
            // A real host started inside a test still shares its authenticated UDP
            // service with the database group. Only simulated same-name fleets isolate.
            crate::liveness::Shared::get(my_id)
        } else {
            let stream = crate::liveness::Shared::isolated(my_id);
            stream.initialize_for_test(hyper_liveness::Settings { local: my_id, run: 1, max_peers: crate::sync::group::MEMBERS, history: hyper_timing::Exposure::new(), resolution: Duration::from_nanos(1) })?;
            stream
        };
        let manager_live = live.clone();
        let shared = Arc::new(Mutex::new(Shared { store: store.clone(), ..Shared::default() }));
        let (ready_tx, ready_rx) = mpsc::channel::<anyhow::Result<()>>();
        let (me_t, tx_t, sh_t, adm_t) = (me.clone(), tx.clone(), Arc::clone(&shared), Arc::clone(&admission));
        let handle = thread::Builder::new()
            .name("kiln-convo-raft".into())
            .spawn(move || match Owner::open(owner_names, live, me_t, my_id, dir, shard_groups, executable, store, out_tx, failover_tx, adm_t, sh_t, tx_t) {
                Ok(owner) => {
                    let _ = ready_tx.send(Ok(()));
                    owner.run(rx);
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            })
            .context("spawning kiln-convo-raft WAL thread")?;
        ready_rx.recv().map_err(|_| anyhow::anyhow!("kiln-convo-raft WAL thread exited during open"))??;
        Ok(Arc::new(Self { live: manager_live, names, executable, me, my_id, policy, tx, admission, thread: Mutex::new(Some(handle)), shared, out_rx, failover_rx }))
    }

    /// Whether opt-in automatic multi-voter failover (`KILN_CONVO_FAILOVER=1`) is enabled.
    pub fn auto_failover(&self) -> bool {
        self.policy.auto_failover
    }

    /// Record that `peer_name` advertised `proto::feature::CONVO_RAFT_V2` (`KRC1` chunked wire
    /// frames and `WireEntry::Part` oversized WAL entries).
    pub fn mark_peer_v2(&self, peer_name: &str) {
        self.set_peer_v2(peer_name, true);
    }

    pub fn set_peer_executor(&self, peer_name: &str, supported: bool) {
        let _ = self.tx.send(Input::SetPeerExecutor(peer_name.to_owned(), supported));
    }

    pub fn set_peer_v2(&self, peer_name: &str, supported: bool) {
        if !peer_name.is_empty() {
            let _ = self.tx.send(Input::SetPeerV2(peer_name.to_owned(), supported));
        }
    }

    /// Cleanly stop the `kiln-convo-raft` WAL owner thread (closing admission first, draining
    /// already-queued proposals durably to WAL, then closing every `convo-raft-*.log` file).
    /// Idempotent; also called automatically on [`Drop`].
    pub fn shutdown(&self) {
        {
            let mut admission = self.admission.lock();
            if !admission.closing {
                admission.closing = true;
                let _ = self.tx.send(Input::Shutdown);
            }
        }
        if let Some(handle) = self.thread.lock().take() {
            let _ = handle.join();
        }
    }

    pub(crate) fn observe_start_holds(&self,holds:&Arc<crate::frontdoor::start_outbox::Outbox>){self.shared.lock().start_holds=Arc::downgrade(holds);}

    /// Listen before reading status so a concurrent custody change wakes every subscriber.
    pub fn changed(&self) -> Arc<crate::rt::Changed> { self.shared.lock().changed.clone() }

    /// Atomically subscribe the single local execution driver and return its initial sessions.
    pub fn watch_custody(&self) -> Vec<String> {
        let mut sh = self.shared.lock();
        sh.custody = Some(CustodyChanges::default());
        sh.statuses.keys().cloned().collect()
    }

    /// Coalesced changes including retirements. A subscriber that exceeded its resource share
    /// takes a fresh baseline; the independent multi-reader status notification remains available.
    pub fn take_custody_changes(&self) -> Option<Vec<String>> {
        let mut sh = self.shared.lock();
        let changes = sh.custody.as_mut()?;
        if changes.rescan { return None; }
        changes.bytes = 0;
        Some(changes.ids.drain().collect())
    }

    /// Ownership transition events (`Claimed` on failover election win, `LostLeadership` on step-down).
    pub fn failover_events(&self) -> crate::memory::queue::Receiver<FailoverEvent> {
        self.failover_rx.clone()
    }

    /// This node's name and Raft member id.
    pub(crate) fn node_name(&self) -> &str { &self.me }

    pub fn identity(&self) -> (String, u64) {
        (self.me.clone(), self.my_id)
    }

    /// Attach or replace the backing conversation store (e.g. after deferred handover store open).
    pub fn set_store(&self, store: Store) {
        let _ = self.tx.send(Input::SetStore(store));
    }

    /// Outgoing Raft frames ready to be delivered to peer nodes (over direct `hyper-quic` or relayed reach).
    pub fn outgoing(&self) -> crate::memory::queue::Receiver<Outgoing> {
        self.out_rx.clone()
    }

    /// Register connected peer nodes in the fleet; any session led by THIS node adds them as Raft
    /// learners so conversations replicate automatically across all connected nodes.
    /// Joined nodes cannot silently bootstrap an independent fleet during an offline first start.
    pub fn can_execute(&self) -> bool { self.executable }

    fn change_names(&self, change: NameChange) -> anyhow::Result<Vec<String>> {
        let (tx, rx) = mpsc::channel();
        self.tx.send(Input::Names(change, tx)).map_err(|_| anyhow::anyhow!("session WAL stopped"))?;
        rx.recv().context("session WAL stopped during namespace handoff")?
    }

    pub fn set_name_registrar(&self, name: &str) -> anyhow::Result<()> {
        if self.names.read().pin.registrar.as_deref() == Some(name) { return Ok(()); }
        self.change_names(NameChange::Registrar(name.to_owned())).map(drop)
    }

    /// Includes live, fenced, parked and archived identities. The owner serializes the
    /// snapshot with new slot creation after the older unrestricted registrar is fenced.
    pub fn historical_name_inventory(&self) -> anyhow::Result<Vec<String>> { self.change_names(NameChange::Inventory) }

    pub fn guard_legacy_names(&self, incomplete: bool, reserved: impl Into<Arc<BTreeSet<String>>>) -> anyhow::Result<()> {
        self.change_names(NameChange::Historical { incomplete, reserved: reserved.into() }).map(drop)
    }

    pub fn freeze_name_registrar(&self) -> anyhow::Result<Vec<String>> { self.change_names(NameChange::Freeze) }

    pub fn require_name_charter(&self, digest: [u8;32]) -> anyhow::Result<()> {
        if self.names.read().pin.charter == Some(digest) { return Ok(()); }
        self.change_names(NameChange::Require(digest)).map(drop)
    }

    pub fn name_charter_digest(&self) -> Option<[u8;32]> { self.names.read().active.as_ref().map(|c| c.digest) }

    pub fn install_name_charter(&self, digest: [u8;32], voters: Vec<String>, reserved: impl Into<Arc<BTreeSet<String>>>) -> anyhow::Result<()> {
        let mut ids: Vec<_> = voters.iter().map(|n| node_id(n)).collect();
        ids.sort_unstable();
        ids.dedup();
        anyhow::ensure!(ids.len() == voters.len() && ids.len() >= MIN_VOTERS && voters.iter().all(|n| !n.is_empty()), "fleet charter has no distinct fault-tolerant electorate");
        self.change_names(NameChange::Install(Arc::new(NameCharter { digest, voters: ids, reserved: reserved.into() }))).map(drop)
    }

    pub fn require_fleet_roster(&self) { self.shared.lock().needs_roster = true; }

    /// Members enrolled but offline still count in a newly opened session's electorate.
    pub fn note_enrolled<'a>(&self, names: impl IntoIterator<Item = &'a str>) {
        self.shared.lock().enrolled.extend(names.into_iter().filter(|name| !name.is_empty()).map(node_id));
    }

    /// Definitive enrollment removal only. Existing sessions retain their original electorate.
    pub fn forget_member(&self, peer: &str) {
        self.shared.lock().enrolled.remove(&node_id(peer));
        self.peer_unreachable(peer);
        self.set_peer_executor(peer, false);
    }

    pub fn register_members(&self, members: Vec<(String, bool, bool)>) {
        if !members.is_empty() {
            self.note_enrolled(members.iter().map(|(name, _, _)| name.as_str()));
            let _ = self.tx.send(Input::RegisterMembers(members));
        }
    }

    pub fn register_peers<I, S>(&self, peer_names: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let names: Vec<String> = peer_names.into_iter().map(Into::into).collect();
        self.note_enrolled(names.iter().map(String::as_str));
        let _ = self.tx.send(Input::RegisterPeers(names));
    }

    /// Mark a peer temporarily unreachable in every active Raft group.
    pub fn peer_unreachable(&self, peer_name: &str) {
        let _ = self.tx.send(Input::PeerUnreachable(peer_name.to_owned()));
    }

    /// Report whether a snapshot frame bundle for `session` to `peer_name` was accepted by transport.
    pub fn report_snapshot(&self, session: &str, peer_name: &str, arrived: bool) {
        // hyper-raft leaves Snapshot on the follower's durable append response at its install index.
        if arrived { return; }
        let _ = self.tx.send(Input::ReportSnapshot {
            session: session.to_owned(),
            peer_name: peer_name.to_owned(),
            arrived,
        });
    }

    /// Count serialized bytes without retaining them. Reserve the command and its frame scratch
    /// before allocating either. The final send and shutdown marker use the same short lock.
    fn enqueue(&self, session: &str, epoch: u64, rows: Vec<Row>, founding: bool, native_move: Option<MoveIntent>, input: impl FnOnce(Vec<PendingProposal>, usize) -> Input) -> anyhow::Result<()> {
        anyhow::ensure!(rows.iter().all(|row| valid_row(row, session, &self.me, epoch)), "invalid session or ownership in proposal for {session}");
        anyhow::ensure!(super::inbox::session_ok(session.as_bytes()).is_some(), "invalid conversation session id");
        let (genesis_members, genesis_charter) = if founding {
            let n = self.names.read();
            let shared = self.shared.lock();
            let exists = shared.statuses.contains_key(session);
            if let Some(charter) = &n.active {
                if charter.reserved.contains(session) {
                    anyhow::ensure!(exists, "session id was reserved before the fleet charter; recover its original WAL and selected node");
                    let registrar = n.pin.registrar.as_ref().context("original name registrar unavailable for reserved session")?;
                    (vec![node_id(registrar)], None)
                } else { (charter.voters.clone(), Some(charter.digest)) }
            } else if n.required {
                anyhow::ensure!(!n.pin.frozen || exists, "fleet name charter is frozen pending verified handoff");
                let registrar = n.pin.registrar.as_ref().context("authenticated fleet name registrar is unavailable")?;
                (vec![node_id(registrar)], None)
            } else {
                anyhow::ensure!(!shared.needs_roster || exists || shared.enrolled.iter().any(|id| *id != self.my_id), "joined fleet membership has not arrived");
                let mut members = shared.enrolled.clone();
                members.insert(self.my_id);
                (members.into_iter().collect(), None)
            }
        } else { (Vec::new(), None) };
        let owner_run=if founding {self.live.with(|l|l.observed_run(self.my_id)).flatten()} else {None};
        let entry = ConvoEntry { process_loss:None, owner_run, reaffirm: false, seal_voters: Vec::new(), genesis_charter, genesis_members, seal: None, opening: founding.then_some(false), session: session.to_owned(), leader: self.me.clone(), epoch, rows, policy: founding.then_some(self.policy), claim_from: None, native_move };
        let raw = crate::memory::queue::json_bytes(&entry);
        let scratch = if raw > ENTRY_DATA_MAX { ENTRY_DATA_MAX.saturating_mul(2) } else { raw };
        let overhead = size_of::<PendingProposal>() + size_of::<(CommitTag, u64, Instant, usize)>() + size_of::<Sender<anyhow::Result<()>>>();
        let charge = raw.checked_add(scratch).and_then(|n| n.checked_add(overhead)).context("proposal size overflow")?;
        reserve_admission(&self.admission, charge).map_err(anyhow::Error::msg)?;
        let result = (|| -> anyhow::Result<()> {
            let mut bytes = Vec::new();
            bytes.try_reserve_exact(raw)?;
            serde_json::to_writer(&mut bytes, &entry)?;
            let mut proposal = PendingProposal::from_entry(bytes);
            proposal.reserved_bytes = charge;
            let message = input(vec![proposal], charge);
            let admission = self.admission.lock();
            anyhow::ensure!(!admission.closing, "conversation Raft WAL manager is shutting down");
            self.tx.send(message).map_err(|_| anyhow::anyhow!("conversation Raft WAL manager stopped"))?;
            Ok(())
        })();
        if result.is_err() {
            release_admission(&self.admission, charge);
            crate::metrics::convo_raft().dropped_proposals.inc();
        }
        result
    }

    /// Non-blocking start (or leadership takeover) of `session`'s WAL-backed Raft group on THIS
    /// node — safe to call from async `hyper-rt` shards without blocking on WAL fsync.
    pub fn start_leader(self: &Arc<Self>, session: &str, epoch: u64, initial_rows: Vec<Row>) {
        let (manager, session) = (self.clone(), session.to_owned());
        crate::rt::spawn(async move {
            let id = session.clone();
            match crate::rt::blocking_in(crate::rt::SHARE_SESSION, move || manager.open_leader(&id, epoch, initial_rows)).await {
                Ok(Ok(_)) => {}
                result => tracing::warn!(%session, ?result, "unable to commit conversation founding operation"),
            }
        });
    }

    /// Start (or take leadership of) `session`'s Raft group on THIS node — called on the runner
    /// where the session is actually started/opened. Persists the election and `initial_rows` to
    /// `convo-raft-*.log`, elects THIS node as the session's initial Raft leader, and replicates to
    /// all known connected fleet peers.
    pub fn open_leader(&self, session: &str, epoch: u64, initial_rows: Vec<Row>) -> anyhow::Result<GroupStatus> {
        self.open_leader_with_seed(session, epoch, initial_rows, Vec::new())
    }

    pub fn open_leader_with_seed(&self, session: &str, epoch: u64, mut initial_rows: Vec<Row>, fresh_inbox_rows: Vec<Row>) -> anyhow::Result<GroupStatus> {
        if self.status(session).is_none_or(|status| !status.founded) {
            let mut history = self.store().context("conversation store unavailable")?.founding_rows(session, epoch, &self.me)?;
            if !history.is_empty() {
                history.append(&mut initial_rows);
                initial_rows = history;
            } else { initial_rows.extend(fresh_inbox_rows); }
        }
        let (reply_tx, reply_rx) = mpsc::channel();
        self.enqueue(session, epoch, initial_rows, true, None, |encoded, bytes| Input::OpenLeader { policy: self.policy, session: session.to_owned(), epoch, encoded, bytes, reply: Some(reply_tx) })?;
        reply_rx.recv().map_err(|_| anyhow::anyhow!("the conversation Raft owner stopped before opening {session}"))?
    }

    /// Propose new conversation rows for `session` on its leader (split up front to WAL entry
    /// bounds and queued for persistence to `convo-raft-*.log`). Returns an immediate error if
    /// shutting down, over queued memory bound, stale ownership epoch, non-leader/non-voter, or
    /// encoding fails.
    pub fn propose(&self, session: &str, epoch: u64, rows: Vec<Row>) -> anyhow::Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        {
            let shared = self.shared.lock();
            let st = shared.statuses.get(session).ok_or_else(|| anyhow::anyhow!("session {session} has no available Raft owner"))?;
            if epoch != st.epoch {
                crate::metrics::convo_raft().dropped_proposals.inc();
                anyhow::bail!("stale ownership epoch {epoch} for session {session} (current={})", st.epoch);
            }
            if !st.is_leader || st.owner.as_deref() != Some(self.me.as_str()) {
                crate::metrics::convo_raft().dropped_proposals.inc();
                anyhow::bail!("node {} is not leader/voter for session {session} (leader={:?})", self.me, st.leader_name);
            }
        }
        self.enqueue(session, epoch, rows, false, None, |encoded, bytes| Input::Propose { session: session.to_owned(), epoch, encoded, bytes, reply: None })
    }

    /// Synchronous variant of [`Manager::propose`] that waits for **true WAL persistence + Raft
    /// quorum commit + state machine apply** (via `StateMachine::Answer` commit tag), returning
    /// `Err` if this node is not leader/voter, epoch is stale, leadership is lost before commit,
    /// or the commit times out.
    pub fn propose_sync(&self, session: &str, epoch: u64, rows: Vec<Row>) -> anyhow::Result<()> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.enqueue(session, epoch, rows, false, None, |encoded, bytes| Input::Propose { session: session.to_owned(), epoch, encoded, bytes, reply: Some(reply_tx) })?;
        reply_rx.recv().map_err(|_| anyhow::anyhow!("the conversation Raft owner stopped before committing for {session}"))?
    }

    /// Commit the staged destination's exact bytes before it can be elected to activate them.
    pub fn move_to(&self, session: &str, target: &str, epoch: u64, digest: String) -> anyhow::Result<()> {
        anyhow::ensure!(valid_digest(&digest), "invalid native move checksum");
        if self.status(session).is_some_and(|s| epoch.checked_add(1) == Some(s.epoch) && s.owner.as_deref() == Some(target) && s.native_move_digest.as_ref() == Some(&digest)) { return Ok(()); }
        let intent = MoveIntent { leadership_only: false, cancel: false, ready: false, epoch, target: target.to_owned(), digest: digest.clone() };
        self.commit_move_intent(session, epoch, intent)?;
        anyhow::ensure!(self.request_transfer(session, target, epoch, Some(digest)), "native move no longer owns this session or target is unreachable");
        Ok(())
    }

    pub fn cancel_move(&self, session: &str, epoch: u64, target: &str, digest: String) -> anyhow::Result<()> {
        self.commit_move_intent(session, epoch, MoveIntent { leadership_only: false, cancel: true, epoch, ready: false, target: target.to_owned(), digest })
    }

    pub fn confirm_native_activation(&self, session: &str, epoch: u64, digest: String) -> anyhow::Result<()> {
        let previous = epoch.checked_sub(1).context("invalid destination epoch")?;
        self.commit_move_intent(session, epoch, MoveIntent { leadership_only: false, cancel: false, epoch: previous, ready: true, target: self.me.clone(), digest })
    }

    fn commit_move_intent(&self, session: &str, epoch: u64, intent: MoveIntent) -> anyhow::Result<()> {
        let (tx, rx) = mpsc::channel();
        self.enqueue(session, epoch, Vec::new(), false, Some(intent), |encoded, bytes| Input::Propose { session: session.to_owned(), epoch, encoded, bytes, reply: Some(tx) })?;
        rx.recv().context("native move has not committed")?
    }

    /// Initiate Raft leadership transfer for `session` to `target_node` (e.g. when a session moves
    /// from `laptop-b` to `laptop-c`). Promotes `target_node` to voter first if it is currently a
    /// learner or absent, then transfers Raft leadership once committed to WAL.
    pub fn transfer_leadership(&self, session: &str, target_node: &str) -> bool {
        self.status(session).is_some_and(|s| self.transfer_at(session, target_node, s.epoch))
    }

    fn transfer_at(&self, session: &str, target_node: &str, epoch: u64) -> bool {
        self.request_transfer(session, target_node, epoch, None)
    }

    fn request_transfer(&self, session: &str, target_node: &str, epoch: u64, digest: Option<String>) -> bool {
        let (reply_tx, reply_rx) = mpsc::channel();
        if self.tx.send(Input::Transfer { epoch, digest, session: session.to_owned(), target_node: target_node.to_owned(), reply: reply_tx }).is_err() {
            return false;
        }
        reply_rx.recv().unwrap_or(false)
    }

    pub async fn begin_move(self: &Arc<Self>, session: &str, target: &str, epoch: u64, digest: &str) -> anyhow::Result<()> {
        let (mgr, id, target, digest) = (self.clone(), session.to_owned(), target.to_owned(), digest.to_owned());
        crate::rt::blocking_in(crate::rt::SHARE_SESSION, move || mgr.move_to(&id, &target, epoch, digest)).await?
    }

    /// Observe the destination's activation without taking a blocking place it may itself
    /// need to commit that activation (including an in-place resume on this machine).
    /// CP-T3.2. Told, never timed: it ends on the destination's activation, another owner's
    /// claim, or the old leader's committed word that it gave the hand-over up.
    pub async fn wait_moved(&self, session: &str, target: &str, old_epoch: u64, digest: &str) -> anyhow::Result<()> {
        let next = old_epoch.checked_add(1).context("session epoch overflow")?;
        let changed = self.changed();
        async {
            loop {
                let notice = changed.listen();
                let status = self.status(session).context("session Raft group disappeared during transfer")?;
                let arrived = status.epoch == next && status.owner.as_deref() == Some(target) && status.leader_id == node_id(target) && status.native_move_digest.as_deref() == Some(digest);
                if arrived && status.native_move_ready { return Ok(()); }
                anyhow::ensure!(arrived || status.epoch <= old_epoch || status.owner.is_none(), "another Raft owner claimed session {session} during transfer");
                let given_up = status.epoch == old_epoch && status.abandoned_move.as_ref().is_some_and(|a| a.epoch == old_epoch && a.target == target && a.digest == digest);
                anyhow::ensure!(!given_up, "its old leader gave the hand-over up");
                notice.await;
            }
        }.await.with_context(|| format!("session {session} has not confirmed ownership on {target}"))
    }

    /// Deliver an incoming Raft message for `session` from transport-authenticated `from_node`.
    pub fn step(&self, from_node: &str, session: &str, bytes: &[u8]) -> anyhow::Result<()> {
        anyhow::ensure!(bytes.len() <= WIRE_PART_MAX, "oversized conversation Raft frame");
        self.step_owned(from_node, session, bytes.to_vec())
    }

    pub fn step_owned(&self, from_node: &str, session: &str, bytes: Vec<u8>) -> anyhow::Result<()> {
        anyhow::ensure!(!from_node.is_empty() && super::inbox::session_ok(session.as_bytes()).is_some() && bytes.len() <= WIRE_PART_MAX, "invalid conversation Raft frame");
        let charge = bytes.capacity().saturating_add(from_node.len()).saturating_add(session.len()).saturating_add(size_of::<Input>());
        let mut admission = self.admission.lock();
        anyhow::ensure!(!admission.closing && admission.incoming.saturating_add(charge) <= crate::memory::room::share(), "conversation Raft receiver is unavailable or full");
        self.tx.send(Input::Step { from_node: from_node.to_owned(), session: session.to_owned(), bytes }).map_err(|_| anyhow::anyhow!("conversation Raft WAL manager stopped"))?;
        admission.incoming += charge;
        Ok(())
    }

    /// Test harness only. Drive work whose actual deadline has arrived on the injected clock.
    #[cfg(test)]
    pub fn drive_due(&self) {
        let _ = self.tx.send(Input::DriveDue);
    }

    /// Wait until all currently submitted WAL writes and off-thread snapshots on this node finish.
    pub fn flush(&self, timeout: Duration) -> bool {
        let (tx, rx) = mpsc::channel();
        if self.tx.send(Input::FlushWait(tx)).is_err() {
            return false;
        }
        rx.recv_timeout(timeout).unwrap_or(false)
    }

    /// Known Raft leader node name for `session`, derived strictly from active groups (never stale).
    pub fn leader_of(&self, session: &str) -> Option<String> {
        self.shared.lock().statuses.get(session).and_then(|st| st.leader_name.clone())
    }

    /// Detailed Raft status for one `session`.
    pub(crate) fn with_status<T>(&self, session: &str, select: impl FnOnce(&GroupStatus) -> T) -> Option<T> {
        self.shared.lock().statuses.get(session).map(select)
    }

    /// Local custody without copying the complete voter and learner sets for every message.
    pub(crate) fn owned_epoch(&self, session: &str) -> Option<u64> {
        self.with_status(session, |s| (s.founded && !s.recovering && !s.execution_held && s.is_leader && s.owner.as_deref() == Some(s.node.as_str())).then_some(s.epoch)).flatten()
    }

    pub fn status(&self, session: &str) -> Option<GroupStatus> {
        self.shared.lock().statuses.get(session).cloned()
    }

    /// Detailed Raft status for every active session on this node (sorted by session id).
    pub fn all_status(&self) -> Vec<GroupStatus> {
        self.shared.lock().statuses.values().cloned().collect()
    }

    /// Summary counts `(total_groups, leading_groups, following_groups)` for `/healthz/server` in O(1).
    pub fn summary(&self) -> (usize, usize, usize) {
        let sh = self.shared.lock();
        let total = sh.statuses.len();
        let leading = sh.leading_count;
        (total, leading, total.saturating_sub(leading))
    }

    /// Backing local conversation store (holds both locally led sessions and Raft-replicated peer sessions).
    pub fn store(&self) -> Option<Store> {
        self.shared.lock().store.clone()
    }

    #[cfg(test)]
    fn force_compact(&self, session: &str, keep: u64) -> anyhow::Result<()> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Input::ForceCompact { session: session.to_owned(), keep, reply: tx })
            .map_err(|_| anyhow::anyhow!("manager stopped"))?;
        rx.recv().map_err(|_| anyhow::anyhow!("force_compact: manager stopped"))?
    }

    #[cfg(test)]
    fn inject_raw_proposal(&self, session: &str, raw: Vec<u8>) -> anyhow::Result<()> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Input::InjectRawProposal { session: session.to_owned(), raw, reply: tx })
            .map_err(|_| anyhow::anyhow!("manager stopped"))?;
        rx.recv().map_err(|_| anyhow::anyhow!("inject_raw_proposal: manager stopped"))?
    }

    #[cfg(test)]
    fn fail_next_persist(&self, session: &str) -> anyhow::Result<()> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Input::FailNextPersist { session: session.to_owned(), reply: tx })
            .map_err(|_| anyhow::anyhow!("manager stopped"))?;
        rx.recv().map_err(|_| anyhow::anyhow!("fail_next_persist: manager stopped"))?
    }

    #[cfg(test)]
    fn snapshot_state(&self, session: &str) -> anyhow::Result<(u32, bool, usize)> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Input::SnapshotState { session: session.to_owned(), reply: tx })
            .map_err(|_| anyhow::anyhow!("manager stopped"))?;
        rx.recv().map_err(|_| anyhow::anyhow!("snapshot_state: manager stopped"))
    }
}

/// Why an open or commit at this group can no longer be confirmed by the members it needs, by
/// the detector's verdicts and the fleet's reports so far; `None` while it still can be. Each
/// verdict that changes it drives the group again (`Owner::node_evidence`), so no waiter is
/// timed.
fn unconfirmable(sg: &SessionGroup, me: u64, peers: &BTreeMap<u64, String>, executors: &BTreeMap<u64, bool>, unreachable: &BTreeSet<u64>) -> Option<&'static str> {
    if sg.local_disk_slow { return Some("this node's WAL is judged slow"); }
    let able = |p: &u64| *p != me && !unreachable.contains(p) && !sg.membership.lost(*p);
    let conf = sg.replica.configuration();
    let machine = sg.replica.machine();
    if conf.voters.len() > 1 && conf.voters.iter().filter(|v| **v == me || able(v)).count() <= conf.voters.len() / 2 {
        return Some("too few of its voters remain for a quorum");
    }
    let copies = || conf.voters.iter().chain(&conf.learners).chain(&sg.wanted_learners).any(able);
    match machine.founding_barrier {
        // As `reconcile_learners` promotes: reachable executors, and in a chartered founding the
        // charter's executors even when unreachable, so long as a majority can answer. Judged
        // once the founding entry has applied, which carries the session's policy and charter.
        FoundingBarrier::VotingFleet if machine.founded => {
            let target = machine.policy.unwrap_or_default().voters();
            let live = peers.keys().chain(&conf.voters).chain(&conf.learners).filter(|p| able(p) && executors.get(*p) == Some(&true)).collect::<BTreeSet<_>>();
            let charter = machine.genesis.as_ref().filter(|g| g.charter.is_some() && machine.founding_index.is_some())
                .map_or(0, |g| g.members.iter().filter(|m| **m != me && !live.contains(m) && executors.get(*m) == Some(&true)).count());
            if live.len().saturating_add(charter).saturating_add(1) < target || live.len().saturating_add(1) <= target / 2 {
                return Some("too few executors remain to form its voters");
            }
        }
        FoundingBarrier::RemoteCopy if !copies() => return Some("no member remains to hold its copy"),
        FoundingBarrier::Local if conf.voters.len() <= 1 && !(conf.learners.is_empty() && sg.wanted_learners.is_empty()) && !copies() => return Some("no member remains to hold its copy"),
        _ => {}
    }
    if !machine.published() && let Some(g) = machine.genesis.as_ref().filter(|_| machine.founding_index.is_some()) {
        let witnesses = g.members.iter().filter(|m| **m == me || sg.genesis_witnesses.contains(*m) || (able(m) && !sg.rejected_witnesses.contains(*m))).count();
        if witnesses <= g.members.len() / 2 { return Some("too few of its name's voters remain to witness it"); }
    }
    None
}

fn settle_committed_tags(session: &str, sg: &mut SessionGroup, committed_tags: &mut Vec<ApplyReceipt>, failovers: &mut Vec<FailoverEvent>, admission: &Mutex<AdmissionState>) {
    for (tag, accepted, index) in committed_tags.drain(..) {
        let charge = sg.credits.remove(&tag).unwrap_or(0);
        if accepted && sg.commit_waiters.contains_key(&tag) {
            sg.applied_waiters.push_back((tag, index, charge));
        } else {
            release_admission(admission, charge);
            if let Some(tx) = sg.commit_waiters.remove(&tag) {
                let _ = tx.send(Err(anyhow::anyhow!("session {session} rejected the committed operation at its ownership fence")));
            }
        }
        if sg.pending_claim.as_ref().is_some_and(|(ct, ..)| *ct == tag)
            && let Some((_, new_epoch, term, prev_leader)) = sg.pending_claim.take()
            && accepted && term == sg.replica.term() && sg.replica.is_leader()
        {
            tracing::info!(%session, previous_leader = %prev_leader, new_epoch, term, "committed automatic conversation Raft failover ownership claim");
            failovers.push(FailoverEvent::Claimed {
                session: session.to_owned(),
                new_epoch,
                previous_leader: prev_leader,
                term,
            });
        }
    }
}

/// Reconcile Raft group membership in two safe phases:
/// 1. Add newly discovered fleet peers as learners (`AddLearnerNode`) so they catch up without
///    disrupting commit quorum.
/// 2. ONLY when opt-in `auto_failover` is enabled (`KILN_CONVO_FAILOVER=1` or `Policy { auto_failover: true }`),
///    promote the caught-up executors in one joint change. A new chartered group can include
///    a temporarily offline voter when a majority of its new configuration already has the log.
///    (Two-node fleets stay 1 voter + 1 learner so 1 offline peer never stalls the sole survivor.)
///
/// Returns `(proposed_conf_change, member_just_committed)`.
fn reconcile_learners(sg: &mut SessionGroup, unreachable_peers: &BTreeSet<u64>, v2_peers: &BTreeSet<u64>, executors: &BTreeMap<u64, bool>, auto_failover: bool) -> (bool, bool) {
    if !sg.replica.is_leader() {
        return (false, false);
    }
    let conf = sg.replica.configuration();
    let mut joined = false;
    if let Some((member, promoting, at)) = sg.changing {
        let done = if promoting {
            conf.voters.contains(&member)
        } else {
            conf.learners.contains(&member) || conf.learners_next.contains(&member)
        };
        if !done && sg.membership.election().is_none_or(|span|at.elapsed() < span) {
            return (false, false);
        }
        joined = done;
        sg.changing = None;
    }
    if conf.voters_outgoing.is_empty() && conf.voters.len() > 1 && let Some(member) = conf.voters.iter().copied().find(|m| executors.get(m) == Some(&false)) {
        let mut changes = vec![ConfChangeSingle { change_type: ConfChangeType::AddLearnerNode, node_id: member }];
        if let Some(replacement) = conf.learners.iter().copied().find(|m| executors.get(m) == Some(&true) && !unreachable_peers.contains(m) && sg.replica.caught_up(*m)) {
            changes.push(ConfChangeSingle { change_type: ConfChangeType::AddNode, node_id: replacement });
        }
        let reconfigure = ConfChangeV2 { transition: ConfChangeTransition::Implicit, changes, context: Vec::new() };
        if sg.replica.change(Vec::new(), &reconfigure).is_ok() {
            sg.changing = Some((member, false, Instant::now()));
            return (true, joined);
        }
        return (false, joined);
    }
    // Phase 1: add any missing peers as learners first.
    let needs_v2 = sg.v2_suffix || sg.replica.machine().chunked_through > sg.compacted || sg.pending_proposals.iter().any(PendingProposal::needs_chunking);
    sg.wanted_learners.retain(|m| !conf.learners.contains(m) && !conf.voters.contains(m));
    let next_learner = sg.wanted_learners.iter().copied().find(|m| (!needs_v2 || v2_peers.contains(m)) && !unreachable_peers.contains(m));
    if let Some(member) = next_learner {
        if sg.replica.change(Vec::new(), &conf_change(member, ConfChangeType::AddLearnerNode)).is_ok() {
            sg.changing = Some((member, false, Instant::now()));
            return (true, joined);
        }
        return (false, joined);
    }
    // Phase 2 (opt-in): promote caught-up reachable learners to voters to establish a fault-tolerant
    // odd quorum, the session policy's 2f + 1 (`Policy::voters`), so leader failure automatically
    // triggers Raft failover.
    let target = sg.replica.machine().policy.unwrap_or_default().voters();
    if auto_failover && conf.voters_outgoing.is_empty() && conf.voters.len() < target {
        let needed = target - conf.voters.len();
        // A fleet smaller than the target may wait here for its last voters for its whole
        // lifetime. Allocate the wire change only when there are enough voters to propose it.
        let ready = |m: &u64| executors.get(m) == Some(&true) && !unreachable_peers.contains(m) && sg.replica.caught_up(*m);
        let learners = || conf.learners.iter().copied().filter(|m| ready(m)).take(needed);
        let machine = sg.replica.machine();
        // The caller has already checked the charter quorum's durable founding witnesses.
        // Raft must also be able to commit under the incoming session voter configuration.
        let current = conf.voters.iter().filter(|v| **v == sg.replica.leader() || (!unreachable_peers.contains(v) && sg.replica.caught_up(**v))).count();
        let charter = || machine.genesis.as_ref()
            .filter(|g| g.charter.is_some() && machine.founding_index.is_some() && current + learners().count() > target / 2)
            .into_iter().flat_map(|g| g.members.iter().copied())
            .filter(|v| !conf.voters.contains(v) && !learners().any(|c| c == *v)
                && executors.get(v) == Some(&true) && (unreachable_peers.contains(v) || v2_peers.contains(v)));
        let candidates = || learners().chain(charter()).take(needed);
        if candidates().count() == needed && let Some(first) = candidates().next() {
            let changes = candidates().map(|node_id| ConfChangeSingle { change_type: ConfChangeType::AddNode, node_id }).collect();
            let change = ConfChangeV2 { transition: ConfChangeTransition::Auto, changes, context: Vec::new() };
            if sg.replica.change(Vec::new(), &change).is_ok() {
                sg.changing = Some((first, true, Instant::now()));
                return (true, joined);
            }
        }
    }
    (false, joined)
}

#[allow(clippy::too_many_arguments)]
fn drive_one(
    session: &str,
    sg: &mut SessionGroup,
    now: u64,
    peers: &BTreeMap<u64, String>,
    v2_peers: &BTreeSet<u64>,
    local_exec: bool,
    output: &mut Output<ApplyReceipt>,
    committed_tags: &mut Vec<ApplyReceipt>,
    out: &crate::memory::queue::Sender<Outgoing>,
    snap_bundles: &mut Vec<SnapshotJob>,
    failed_snapshots: &mut Vec<(String, u64)>,
    next_wire_id: &mut u64,
) -> Result<u64, ReplicaError> {
    let mut chunk_frames = 0u64;
    for _ in 0..DRIVE_STEPS_MAX {
        output.clear();
        let previous_handover=sg.replica.handed_over();
        let driven = sg.replica.drive(now, &sg.waker, output)?;
        let prior=sg.replica.machine();
        if sg.replica.handed_over()!=previous_handover && sg.replica.is_leader() && prior.owner_term==sg.replica.term()
            && node_id(&prior.leader_name)==sg.replica.id() {
            // Per-log disk patience can expire before the node's next poll. Remember
            // the incumbent before publishing the new log leader on an arriving vote.
            sg.held_owner=Some(sg.replica.id());
        }
        sg.membership.io(&driven);
        if let Err(error) = sg.membership.reconcile(&mut sg.replica,false) { tracing::warn!(%session,%error,"applying session detector membership failed"); }
        sg.writes_out = driven.out;
        committed_tags.append(&mut output.answers);
        for mut m in output.messages.drain(..) {
            let machine = sg.replica.machine();
            if m.context.is_empty() && let Some(g) = &machine.genesis {
                let durable = machine.may_publish(g) && !m.reject && machine.durable.index >= g.index && m.commit >= g.index && matches!(m.msg_type, MessageType::MsgAppendResponse | MessageType::MsgHeartbeatResponse);
                let witness = durable && machine.name_vote == Some(g.digest);
                m.context.reserve_exact(36);
                m.context.extend_from_slice(if witness { b"KGW2" } else if durable { b"KGA2" } else if machine.published() { b"KGS1" } else { b"KGR1" });
                m.context.extend_from_slice(&g.digest);
            }
            // A former executor keeps voting until its demotion commits; it cannot solicit
            // votes for itself while no local session host can take custody.
            if !local_exec && matches!(m.msg_type, MessageType::MsgRequestVote | MessageType::MsgRequestPreVote) { continue; }
            let to_id = m.to;
            let is_snap = m.msg_type == MessageType::MsgSnapshot;
            if let Some(to_name) = peers.get(&to_id) {
                // A reconnected member may have downgraded since this log suffix was written.
                if !v2_peers.contains(&to_id) && m.entries.iter().any(|e| e.data.starts_with(br#"{"chunk_id":"#)) { continue; }
                let bytes = m.encoded_len();
                let chunked = bytes > WIRE_PART_MAX;
                if chunked && !v2_peers.contains(&to_id) {
                    if is_snap { failed_snapshots.push((session.to_owned(), to_id)); }
                    continue;
                }
                let id = *next_wire_id;
                *next_wire_id = next_wire_id.wrapping_add(1).max(1);
                if is_snap {
                    let parts = if chunked { bytes.div_ceil(WIRE_PART_MAX - WIRE_CHUNK_HDR) } else { 1 };
                    let Ok(total) = u32::try_from(parts) else {
                        failed_snapshots.push((session.to_owned(), to_id));
                        continue;
                    };
                    if chunked { chunk_frames = chunk_frames.saturating_add(u64::from(total)); }
                    snap_bundles.push(SnapshotJob { message: m, session: session.to_owned(), to_name: to_name.clone(), path: sg.replica.machine().state_file.clone(), id, total });
                } else {
                    let encoded = m.encode_to_vec();
                    for bytes in encode_wire_frames(id, encoded) {
                        let _ = out.try_send(Outgoing { session: session.to_owned(), to_id, to_name: to_name.clone(), bytes, is_snapshot: false, snapshot_last: false });
                    }
                }
            }
        }
        // In quiet groups there may be no core response after the vote record or the
        // commit itself is flushed. Send the proof once when it becomes available.
        let machine = sg.replica.machine();
        let leader = sg.replica.leader();
        let durable = sg.replica.durable_commit();
        if leader != 0 && !sg.replica.is_leader()
            && (machine.founding_index.is_some() || sg.replica.configuration().voters.len() == 1)
            && let Some(g) = machine.genesis.as_ref()
            && durable >= g.index && machine.durable.index >= g.index && machine.may_publish(g)
            && let Some(to) = peers.get(&leader) {
            let witness = machine.name_vote == Some(g.digest);
            let point = (leader,sg.replica.term(),durable,witness);
            if sg.sent_proof != Some(point) {
                let mut ack = hyper_raft::proto::message(leader,MessageType::MsgHeartbeatResponse);
                ack.from = sg.replica.id();
                ack.term = sg.replica.term();
                ack.commit = durable;
                ack.context = Vec::with_capacity(36);
                ack.context.extend_from_slice(if witness { b"KGW2" } else { b"KGA2" });
                ack.context.extend_from_slice(&g.digest);
                match out.try_send(Outgoing { session: session.to_owned(), to_id: leader, to_name: to.clone(), bytes: ack.encode_to_vec(),
                    is_snapshot:false, snapshot_last:false }) {
                    Ok(()) => sg.sent_proof = Some(point),
                    Err(e) => { use crate::memory::queue::Weigh; sg.blocked_proof = e.into_inner().weight(); }
                }
            }
        }
        // The final append can leave before the seal applies. Announce its digest
        // once per leader term so a rival provisional founder can request its image.
        if sg.replica.is_leader() && sg.replica.machine().published() && sg.publication_term!=Some(sg.replica.term()) {
            sg.publication_term=Some(sg.replica.term());
            sg.replica.beat()?;
            continue;
        }
        if !driven.more {
            break;
        }
    }
    Ok(chunk_frames)
}

fn group_status(session: &str, me: &str, my_id: u64, sg: &SessionGroup, peers: &BTreeMap<u64, String>) -> GroupStatus {
    let conf = sg.replica.configuration();
    let recovering = sg.replica.fenced().is_some();
    let leader_id = if recovering { 0 } else { sg.replica.leader() };
    let leader_name = peers.get(&leader_id).cloned();
    let machine = sg.replica.machine();
    let native = machine.native_move.as_ref().filter(|m| m.epoch.checked_add(1) == Some(machine.epoch) && m.target == machine.leader_name && valid_digest(&m.digest));
    let name_conflict = sg.conflicting_names();
    #[derive(Deserialize)]
    struct OriginalStart { start_request: Option<String> }
    let conflicting_start_request = name_conflict.then(|| machine.applied_session.as_ref()?.info.as_ref())
        .flatten().and_then(|info| serde_json::from_str::<OriginalStart>(info).ok()?.start_request);
    let current_owner = node_id(&machine.leader_name);
    let owner_disk_slow = sg.replica.disk_is_slow(current_owner) || current_owner == my_id && sg.local_disk_slow;
    let execution_held = !recovering && machine.published() && (leader_id != 0 || sg.held_owner == Some(my_id) && sg.has_process_quorum(my_id))
        && (leader_id != current_owner || machine.owner_term != sg.replica.term() || owner_disk_slow)
        && sg.held_owner == Some(node_id(&machine.leader_name)) && sg.lost_owner != sg.held_owner && sg.pending_claim.is_none();
    GroupStatus {
        execution_held, name_conflict, conflicting_start_request,
        recovering,
        native_move_digest: native.map(|m| m.digest.clone()),
        native_move_ready: native.is_some_and(|m| m.ready),
        abandoned_move: machine.abandoned_move.as_ref().filter(|m| m.epoch == machine.epoch)
            .map(|m| AbandonedMove { epoch: m.epoch, target: m.target.clone(), digest: m.digest.clone() }),
        founded: machine.published(),
        session: session.to_owned(),
        node: me.to_owned(),
        node_id: my_id,
        is_leader: !recovering && sg.replica.is_leader(),
        owner: (execution_held || (!recovering && !owner_disk_slow && machine.published() && machine.owner_term == sg.replica.term()
            && leader_id == node_id(&machine.leader_name) && sg.pending_claim.is_none())).then(|| machine.leader_name.clone()),
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

    #[test]
    fn cp_t32_receipts_are_scoped_to_member_and_elected_term() {
        let first = CommitTag { member: 1, term: 9, sequence: 1 };
        let op = CommitTag { member: 2, term: 9, sequence: 1 };
        assert_ne!(CommitTag::from_entry(&first.context(), 9), Some(op));
        assert_eq!(CommitTag::from_entry(&op.context(), 9), Some(op));
        assert_eq!(CommitTag::from_entry(&op.context(), 10), None);
        assert_eq!(CommitTag::from_entry(&1u64.to_le_bytes(), 9), None);
    }

    #[test]
    fn cp_t32_source_spool_detects_valid_json_corruption_without_changing_wire_frames() -> anyhow::Result<()> {
        use std::io::{Seek as _, Write as _};
        let dir = tempfile::tempdir()?;
        let mut bytes = br#"{"data":"A"#.to_vec();
        bytes.resize(WIRE_PART_MAX + 1, b'x');
        bytes.extend_from_slice(br#""}"#);
        let offset = bytes.iter().position(|&b| b == b'A').context("missing test byte")?;
        let source = SnapshotSource::new(bytes.clone(), &dir.path().join("state"))?;
        let mut send = SnapshotSend { session: "spool".into(), to_name: "peer".into(), to_id: 2, index: 3, term: 1, id: 4, part: 0, total: 2, source };
        let previous_format = encode_wire_frames(send.id, bytes.clone());
        for (i, expected) in previous_format.iter().enumerate() {
            send.part = u32::try_from(i)?;
            anyhow::ensure!(&send.frame()?.bytes == expected);
        }
        let SnapshotSource::Spool { file, .. } = &mut send.source else { anyhow::bail!("expected spilled snapshot"); };
        file.seek(std::io::SeekFrom::Start(u64::try_from(offset)?))?;
        file.write_all(b"B")?;
        *bytes.get_mut(offset).context("missing mutation")? = b'B';
        anyhow::ensure!(serde_json::from_slice::<serde_json::Value>(&bytes).is_ok());
        send.part = 0;
        anyhow::ensure!(send.frame().is_err());
        Ok(())
    }

    #[test]
    fn cp_t32_saturated_snapshot_queue_waits_before_allocating_or_reading_another_frame() -> anyhow::Result<()> {
        use crate::memory::queue::Weigh;
        let dir = tempfile::tempdir()?;
        let frame = Outgoing { session: "queue".into(), to_name: "receiver".into(), to_id: 2, bytes: vec![0; WIRE_PART_MAX], is_snapshot: true, snapshot_last: false };
        // There is free space after the first frame, but too little for the next.
        let (queue, rx) = crate::memory::queue::bounded(&crate::memory::owners::CHANNELS, frame.weight() + WIRE_PART_MAX / 2);
        queue.try_send(frame).map_err(|_| anyhow::anyhow!("cannot fill transport queue"))?;
        let source = SnapshotSource::new(vec![7; 2 * (WIRE_PART_MAX - WIRE_CHUNK_HDR)], &dir.path().join("state"))?;
        let mut send = SnapshotSend { session: "queue".into(), to_name: "receiver".into(), to_id: 2, index: 3, term: 1, id: 4, part: 0, total: 2, source };
        let measured = crate::os::counted::ProcessWindow::begin();
        let before = crate::os::counted::made();
        for _ in 0..65_536 { assert!(!std::hint::black_box(&send).has_credit(&queue)); }
        let after = crate::os::counted::made();
        if let Some(window) = measured { window.report("saturated queue 65536 admission checks"); }
        assert_eq!(before, after, "no allocation or reallocation for a blocked disk frame");
        let measured = crate::os::counted::ProcessWindow::begin();
        let begin = Instant::now();
        for frame in 0..64 {
            drop(rx.try_recv()?);
            anyhow::ensure!(send.has_credit(&queue));
            send.part = frame % 2;
            queue.try_send(send.frame()?).map_err(|_| anyhow::anyhow!("released frame credit was unusable"))?;
        }
        let elapsed = begin.elapsed();
        if let Some(window) = measured {
            window.report("queue disk read, send and release");
            eprintln!("queue transfer: {} frame bytes in {} microseconds", 64 * WIRE_PART_MAX, elapsed.as_micros());
        }
        Ok(())
    }

    #[test]
    fn cp_t32_wal_move_retries_only_its_own_verified_generation() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let from = root.path().join("blue/wal");
        let to = root.path().join("green/wal");
        std::fs::create_dir_all(&from)?;
        let live = wal_lock(&from.join(".wal-owner.lock"))?;
        anyhow::ensure!(move_wal_dir_checked(&from, &to, true).is_err());
        drop(live);
        move_wal_dir_checked(&from, &to, true)?;
        move_wal_dir_checked(&from, &to, true)?;
        std::fs::write(to.join(".wal-generation"), "different")?;
        anyhow::ensure!(move_wal_dir_checked(&from, &to, true).is_err());
        Ok(())
    }

    /// Multi-node fleet harness backed by REAL per-node `hyper-log` WAL files (`convo-raft-NNN.log`)
    /// and REAL Turso conversation `Store`s, routing per-session Raft messages deterministically.
    struct DetectorLinks {
        wire:kiln_sim::Queue<(u64,u64,Vec<u8>)>,
        rng:kiln_sim::Rng,
        flushing:BTreeMap<u64,u64>,
        alarms:BTreeMap<u64,u64>,
    }

    struct Fleet {
        detector_links:Mutex<DetectorLinks>,
        detector_cut:AtomicBool,
        offline:Mutex<BTreeSet<String>>,
        snapshot_bytes: AtomicU64,
        policy: Policy,
        nodes: BTreeMap<String, (Arc<Manager>, Store, tempfile::TempDir, tempfile::TempDir)>,
    }

    impl Fleet {
        fn new(names: &[&str]) -> anyhow::Result<Self> {
            Self::new_with_policy(names, Policy::default())
        }

        fn new_with_policy(names: &[&str], policy: Policy) -> anyhow::Result<Self> {
            let mut nodes = BTreeMap::new();
            let all: Vec<String> = names.iter().map(|s| (*s).to_owned()).collect();
            for name in &all {
                let (store_dir, store) = crate::convo::store::scratch()?;
                let wal_dir = tempfile::tempdir()?;
                let mgr = Manager::open_with_policy(name.clone(), wal_dir.path(), Some(store.clone()), policy)?;
                mgr.register_peers(all.iter().cloned());
                for p in &all {
                    mgr.mark_peer_v2(p);
                    mgr.set_peer_executor(p, true);
                }
                nodes.insert(name.clone(), (mgr, store, store_dir, wal_dir));
            }
            Ok(Self { detector_links:Mutex::new(DetectorLinks{wire:kiln_sim::Queue::new(),rng:kiln_sim::Rng::new(19),flushing:BTreeMap::new(),alarms:BTreeMap::new()}), detector_cut:AtomicBool::new(false), offline:Mutex::default(), snapshot_bytes: AtomicU64::new(0), policy, nodes })
        }

        fn add_node(&mut self, name: &str) -> anyhow::Result<()> {
            let (store_dir, store) = crate::convo::store::scratch()?;
            let wal_dir = tempfile::tempdir()?;
            let mgr = Manager::open_with_policy(name.to_owned(), wal_dir.path(), Some(store.clone()), self.policy)?;
            self.nodes.insert(name.to_owned(), (mgr, store, store_dir, wal_dir));
            let all: Vec<String> = self.nodes.keys().cloned().collect();
            for (m, _, _, _) in self.nodes.values() {
                m.register_peers(all.iter().cloned());
                for p in &all {
                    m.mark_peer_v2(p);
                    m.set_peer_executor(p, true);
                }
            }
            Ok(())
        }

        fn while_pumping<T: Send>(&self, call: impl FnOnce() -> anyhow::Result<T> + Send) -> anyhow::Result<T> {
            thread::scope(|scope| {
                let job = scope.spawn(call);
                let until = Instant::now() + TEST_WAIT + (TEST_WAIT / 2);
                let mut pumped = Ok(());
                while !job.is_finished() && Instant::now() < until && pumped.is_ok() { pumped = self.settle(); }
                // A caller waits on its owner's answer, never a timer. One still waiting here is a
                // failure: stopping the owners drops what it waits on, so it wakes and the test fails
                // rather than hangs.
                let stuck = !job.is_finished();
                if stuck { for (mgr, _, _, _) in self.nodes.values() { mgr.shutdown(); } }
                let answer = job.join().map_err(|_| anyhow::anyhow!("fleet operation stopped"))?;
                pumped?;
                anyhow::ensure!(!stuck, "fleet operation was not answered while the fleet was pumped: {:?}", answer.as_ref().err());
                answer
            })
        }

        fn open(&self, node: &str, session: &str, epoch: u64, rows: Vec<Row>) -> anyhow::Result<GroupStatus> {
            self.while_pumping(|| self.mgr(node)?.open_leader(session, epoch, rows))
        }

        fn mgr(&self, name: &str) -> anyhow::Result<&Arc<Manager>> {
            self.nodes.get(name).map(|(m, _, _, _)| m).ok_or_else(|| anyhow::anyhow!("missing node {name}"))
        }

        fn store(&self, name: &str) -> anyhow::Result<&Store> {
            self.nodes.get(name).map(|(_, s, _, _)| s).ok_or_else(|| anyhow::anyhow!("missing store {name}"))
        }

        fn wal_dir(&self, name: &str) -> anyhow::Result<&Path> {
            self.nodes.get(name).map(|(_, _, _, w)| w.path()).ok_or_else(|| anyhow::anyhow!("missing wal_dir {name}"))
        }

        /// Deterministic message-pump + WAL-flush settlement (asserts convergence without sleep loops).
        fn settle(&self) -> anyhow::Result<()> {
            self.settle_except(&[])
        }

        /// Deterministic message-pump + WAL-flush settlement while simulating `offline` nodes
        /// crashed/partitioned (their outgoing messages are dropped and nothing is delivered to them).
        fn settle_except(&self, offline: &[&str]) -> anyhow::Result<()> {
            let healed={
                let mut old=self.offline.lock();
                let healed=old.iter().any(|p|!offline.contains(&p.as_str()));
                old.retain(|p|offline.contains(&p.as_str()));
                for &p in offline {if !old.contains(p){old.insert(p.to_owned());}}
                healed
            };
            if healed && self.detector_cut.swap(false,Ordering::SeqCst) {self.detector_partition(offline,false)?;}
            let mut remaining_deadlines = self.nodes.len();
            for _ in 0..512 {
                for (name, (mgr, _, _, _)) in &self.nodes {
                    if offline.contains(&name.as_str()) {
                        continue;
                    }
                    anyhow::ensure!(mgr.flush(TEST_WAIT), "node {name} WAL flush stalled");
                }
                let mut moved = false;
                for (from, (mgr, _, _, _)) in &self.nodes {
                    if offline.contains(&from.as_str()) {
                        continue;
                    }
                    let rx = mgr.outgoing();
                    while let Ok(msg) = rx.try_recv() {
                        moved = true;
                        if std::env::var_os("KILN_TEST_RAFT_REPAIR").is_some() && let Ok(m)=Message::decode(&msg.bytes) {
                            eprintln!("Raft test {from}->{} {:?} ctx {:?}, idx {}, commit {}, snapshot request {}, offline {:?}",msg.to_name,m.msg_type,m.context.get(..4),m.index,m.commit,m.request_snapshot,offline);
                        }
                        let target_online = !offline.contains(&msg.to_name.as_str());
                        if target_online
                            && let Some((target, _, _, _)) = self.nodes.get(&msg.to_name)
                        {
                            target.step(from, &msg.session, &msg.bytes)?;
                            if msg.is_snapshot { self.snapshot_bytes.fetch_add(u64::try_from(msg.bytes.len())?, Ordering::Relaxed); }
                            if msg.is_snapshot && msg.snapshot_last {
                                mgr.report_snapshot(&msg.session, &msg.to_name, true);
                            }
                        } else if msg.is_snapshot {
                            mgr.report_snapshot(&msg.session, &msg.to_name, false);
                        }
                    }
                }
                if moved {
                    continue;
                }
                if remaining_deadlines > 0 {
                    remaining_deadlines -= 1;
                    self.advance(offline)?;
                    continue;
                }
                return Ok(());
            }
            anyhow::bail!("fleet did not settle within 512 deterministic rounds")
        }

        /// Advance every node to the first real Raft deadline on their common
        /// clock. Deliver votes at that point before the next candidate's draw.
        fn advance(&self,offline:&[&str])->anyhow::Result<()> {
            let online=||self.nodes.iter().filter(|(n,_)| !offline.contains(&n.as_str())).map(|(_,g)|&g.0);
            let mut next=None;
            for m in online() {
                let (ask,answer)=mpsc::channel();
                m.tx.send(Input::NextTimer(ask)).map_err(|_|anyhow::anyhow!("Raft timer owner stopped"))?;
                if let Some(t)=answer.recv_timeout(TEST_WAIT)? {next=Some(next.map_or(t,|old:u64|old.min(t)));}
            }
            for m in online() {
                if let Some(at)=next {m.live.set_test_clock(at.max(m.live.now_ns()));}
                m.drive_due();
            }
            Ok(())
        }

        /// Drive the actual node detectors on a virtual link with kernel arrival stamps,
        /// a measurable echo round trip and log flushes. Cut nodes stop sending evidence.
        fn detector_partition(&self, offline: &[&str], isolate: bool) -> anyhow::Result<()> {
            self.detect_until(offline, isolate, || true)
        }

        fn detect_until(&self, offline: &[&str], isolate: bool, observed: impl Fn() -> bool) -> anyhow::Result<()> {
            self.detect_links(offline, isolate, &[], observed)
        }

        fn detect_links(&self, offline: &[&str], isolate: bool, broken: &[(u64,u64)], observed: impl Fn() -> bool) -> anyhow::Result<()> {
            use crate::sim::session_liveness::Beats;
            if !offline.is_empty() {
                self.detector_cut.store(true, Ordering::SeqCst);
            }
            let links: Vec<_> = self.nodes.iter().map(|(name, (mgr, _, _, _))| (name, mgr)).collect();
            let mut now = links.iter().map(|(_, m)| m.live.now_ns()).max().unwrap_or(1);
            // Keep in-flight proof writes, UDP packets and absolute alarms between
            // fault transitions. Dropping those here would itself simulate another cut.
            let mut network=self.detector_links.lock();
            let DetectorLinks{wire,rng,flushing,alarms}=&mut *network;
            let mut warm = false;
            for (_,m) in &links {alarms.insert(m.my_id,now);}
            let due_limit = now + u64::try_from(Duration::from_secs(60).as_nanos())?;
            while now < due_limit {
                let alarm = alarms.values().min().copied();
                let arrival = wire.next_at().map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
                let next = alarm.into_iter().chain(arrival).min().context("no detector wake")?;
                now = now.max(next) + if Some(next) == alarm { rng.below(20_000) } else { 0 };
                let mut arrived = BTreeMap::<u64, Vec<(u64, u64, Vec<u8>)>>::new();
                while let Some((stamp, (from, to, data))) = wire.pop_due(Duration::from_nanos(now)) {
                    arrived.entry(to).or_default().push((from, u64::try_from(stamp.as_nanos())?, data));
                }
                for (name, mgr) in &links {
                    mgr.live.set_test_clock(now);
                    let id = mgr.my_id;
                    if warm && offline.contains(&name.as_str()) && !isolate {
                        alarms.remove(&id);
                        continue;
                    }
                    let packets = arrived.remove(&id).unwrap_or_default();
                    if alarms.get(&id).is_some_and(|at| *at > now) && packets.is_empty() {
                        continue;
                    }
                    let mut beat = Beats::default();
                    let mut disk_changed = false;
                    let next = mgr
                        .live
                        .with(|l| {
                            if flushing.get(&id).is_some_and(|at| now >= *at + 2_000_000)
                                && let Some(start) = flushing.remove(&id)
                            {
                                l.on_durable(hyper_liveness::Write::Liveness, start, now);
                            }
                            for (from, stamp, data) in packets {
                                let _ = l.on_heartbeat(from, &data, stamp, &mut beat);
                            }
                            if let Some(due) = l.wake()
                                && now >= due
                            {
                                l.on_wait(due, now);
                            }
                            let was_slow = l.own_disk_slow();
                            l.poll(now, &mut beat);
                            disk_changed = l.own_disk_slow() != was_slow;
                            l.wake()
                        })
                        .flatten();
                    if disk_changed {
                        mgr.live.notify(crate::liveness::Event::LocalDisk);
                    }
                    if beat.flush {
                        flushing.entry(id).or_insert(now);
                    }
                    for c in beat.changes {
                        mgr.live.notify(crate::liveness::Event::Change(c));
                    }
                    for (to, data) in beat.messages {
                        if broken.contains(&(id,to)) {continue;}
                        if warm
                            && let Some((peer, _)) = links.iter().find(|(_, m)| m.my_id == to)
                            && (offline.contains(&name.as_str()) != offline.contains(&peer.as_str()))
                        {
                            continue;
                        }
                        anyhow::ensure!(
                            wire.push(Duration::from_nanos(now + 200_000), (id, to, data)),
                            "sim network full"
                        );
                    }
                    if !warm {
                        for (_, peer) in &links {
                            if peer.my_id != id {
                                mgr.live.notify(crate::liveness::Event::Timing(peer.my_id));
                            }
                        }
                    }
                    if let Some(at) = next
                        .into_iter()
                        .chain(flushing.get(&id).map(|s| s + 2_000_000))
                        .filter(|t| *t > now)
                        .min()
                    {
                        alarms.insert(id, at);
                    } else {
                        alarms.remove(&id);
                    }
                }
                if !warm {
                    warm = links.iter().all(|(_, m)| {
                        links.iter().all(|(_, other)| {
                            if other.my_id == m.my_id || broken.contains(&(other.my_id,m.my_id)) || other.live.with(|l| l.report(m.my_id).is_none()) == Some(true) {
                                return true;
                            }
                            m.live.with(|l| {
                                l.report(other.my_id).is_none_or(|r| {
                                    r.freshness.is_some()
                                        && r.judged
                                        && l.flush_tail().is_some()
                                        && matches!(l.trust(other.my_id), Some(hyper_timing::Trust::Trusted { .. }))
                                })
                            }) == Some(true)
                        })
                    });
                } else {
                    let complete = links
                        .iter()
                        .filter(|(n, _)| isolate == offline.contains(&n.as_str()))
                        .all(|(_, m)| {
                            links
                                .iter()
                                .filter(|(n, _)| isolate != offline.contains(&n.as_str()))
                                .all(|(_, peer)| {
                                    m.live.with(|l| {
                                        l.trust(peer.my_id)
                                            .is_none_or(|trust| trust == hyper_timing::Trust::Suspected)
                                    }) == Some(true)
                                })
                        });
                    if complete && observed() {
                        for (_, m) in links {
                            m.drive_due();
                        }
                        return Ok(());
                    }
                }
            }
            anyhow::bail!("virtual detectors did not observe the partition")
        }

        fn elect_survivor(&self, offline: &[&str], session: &str) -> anyhow::Result<()> {
            self.detector_partition(offline, false)?;
            let until = Instant::now() + TEST_WAIT + (TEST_WAIT / 2);
            loop {
                self.settle_except(offline)?;
                if self
                    .nodes
                    .iter()
                    .filter(|(n, _)| !offline.contains(&n.as_str()))
                    .any(|(n, g)| {
                        g.0.status(session)
                            .is_some_and(|s| s.is_leader && s.owner.as_ref() == Some(n))
                    })
                {
                    return Ok(());
                }
                anyhow::ensure!(
                    Instant::now() < until,
                    "surviving quorum did not commit its custody: {:?}",
                    self.nodes.iter().map(|(n, g)| (n, g.0.status(session))).collect::<Vec<_>>()
                );
                self.advance(offline)?;
            }
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
        let st = fleet.open("laptop-b", "sess-1", 1, vec![open_row.clone()])?;
        anyhow::ensure!(st.is_leader && st.leader_name.as_deref() == Some("laptop-b"), "laptop-b must be initial Raft leader: {st:?}");

        let ev = user_event("sess-1", 1, "hello from laptop-b");
        fleet.store("laptop-b")?.put(vec![open_row, ev.clone()]);
        fleet.mgr("laptop-b")?.propose("sess-1", 1, vec![ev])?;
        fleet.settle()?;

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
        fleet.open("laptop-b", "sess-on-b", 1, vec![user_event("sess-on-b", 1, "b work")])?;
        fleet.open("laptop-a", "sess-on-a", 1, vec![user_event("sess-on-a", 1, "a work")])?;
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
        anyhow::ensure!(a_kept.as_ref().is_some_and(|k| k.events.len() == 1), "laptop-a sess-on-b kept={a_kept:?}");
        let b_kept = fleet.store("laptop-b")?.kept("sess-on-a", 10)?;
        anyhow::ensure!(b_kept.as_ref().is_some_and(|k| k.events.len() == 1), "laptop-b sess-on-a kept={b_kept:?}");
        anyhow::ensure!(fleet.store("server")?.kept("sess-on-a", 10)?.is_some() && fleet.store("server")?.kept("sess-on-b", 10)?.is_some());
        Ok(())
    }

    #[test]
    fn cp_t32_stale_abc_and_expanded_abcde_directories_share_one_name_quorum() -> anyhow::Result<()> {
        let fleet = Fleet::new(&["a","b","c","d","e"])?;
        let voters: Vec<String> = ["a","b","c"].into_iter().map(str::to_owned).collect();
        let charter = [9;32];
        for (manager, _, _, _) in fleet.nodes.values() {
            manager.set_name_registrar("original-registrar")?;
            manager.install_name_charter(charter, voters.clone(), BTreeSet::new())?;
        }
        // D/E are new enrolled executors. They cannot count as substitute name witnesses
        // even if their local directory regards CDE as a majority of ABCDE.
        for node in ["c","d","e"] {
            let m = fleet.mgr(node)?;
            m.register_peers(["a","b","c","d","e"]);
            anyhow::ensure!(m.install_name_charter([8;32], ["c","d","e"].into_iter().map(str::to_owned).collect(), BTreeSet::new()).is_err());
        }
        let session = "explicit-across-rosters";
        let minority = fleet.mgr("d")?.clone();
        let pending = thread::spawn(move || minority.open_leader(session, 1, vec![user_event(session, 1, "CDE")]));
        let until = Instant::now() + TEST_WAIT;
        while fleet.store("c")?.kept(session, 1)?.is_none_or(|k| k.events.is_empty()) && Instant::now() < until { fleet.settle_except(&["a","b"])?; }
        anyhow::ensure!(!pending.is_finished() && fleet.mgr("d")?.status(session).is_some_and(|st| !st.founded), "a disjoint directory majority acknowledged the id");
        let majority = fleet.mgr("a")?.clone();
        let winner = thread::spawn(move || majority.open_leader(session, 1, vec![user_event(session, 1, "AB")]));
        let until = Instant::now() + TEST_WAIT;
        while !winner.is_finished() && Instant::now() < until { fleet.settle_except(&["c","d","e"])?; }
        winner.join().map_err(|_| anyhow::anyhow!("charter owner stopped"))??;
        // The provisional founder is told once it hears the winner: no timer answers it. Its
        // groups beat once the detector has measured their links.
        fleet.detector_partition(&[], false)?;
        let until = Instant::now() + TEST_WAIT;
        while !pending.is_finished() && Instant::now() < until { fleet.settle()?; }
        anyhow::ensure!(pending.is_finished(), "the provisional founder must be told once it hears the winner");
        anyhow::ensure!(pending.join().map_err(|_| anyhow::anyhow!("provisional founder stopped"))?.is_err());
        Ok(())
    }

    #[test]
    fn cp_t32_name_freeze_preserves_historic_singletons_and_refuses_new_reserved_ids() -> anyhow::Result<()> {
        let (_db, db) = crate::convo::store::scratch()?;
        let wal = tempfile::tempdir()?;
        let original = Manager::open_for_host("registrar", wal.path(), Some(db.clone()), Policy::default(), true)?;
        anyhow::ensure!(original.open_leader("unbound", 1, vec![]).is_err());
        original.set_name_registrar("registrar")?;
        original.open_leader("historic", 1, vec![])?;
        let reserved: BTreeSet<_> = original.freeze_name_registrar()?.into_iter().collect();
        anyhow::ensure!(reserved.contains("historic"));
        original.shutdown();
        let original = Manager::open_for_host("registrar", wal.path(), Some(db), Policy::default(), true)?;
        anyhow::ensure!(original.open_leader("new-during-cutoff", 1, vec![]).is_err());
        let voters: Vec<String> = ["a","b","c"].into_iter().map(str::to_owned).collect();
        let digest = [4;32];
        original.install_name_charter(digest, voters.clone(), reserved.clone())?;
        anyhow::ensure!(original.status("historic").is_some_and(|s| s.founded));
        let fleet = Fleet::new(&["a","b","c"])?;
        for (member, _, _, _) in fleet.nodes.values() {
            member.set_name_registrar("registrar")?;
            member.require_name_charter(digest)?;
            member.install_name_charter(digest, voters.clone(), reserved.clone())?;
        }
        anyhow::ensure!(fleet.mgr("a")?.open_leader("historic", 1, vec![]).is_err());
        Ok(())
    }

    #[test]
    fn cp_t32_pre_cutoff_start_absent_from_signed_history_reuses_its_original_opening() -> anyhow::Result<()> {
        let fleet = Fleet::new(&["a","b","c","registrar"])?;
        for (m,_,_,_) in fleet.nodes.values() { m.set_name_registrar("registrar")?; }
        let session = "cutoff-before-registrar-contact";
        let original = fleet.mgr("a")?.clone();
        let opening = thread::spawn(move || original.open_leader(session, 1, vec![user_event(session, 1, "original prompt")]));
        let until = Instant::now() + TEST_WAIT;
        while fleet.store("c")?.kept(session, 1)?.is_none_or(|s| s.events.is_empty()) && Instant::now() < until {
            fleet.settle_except(&["registrar"])?;
        }
        anyhow::ensure!(!opening.is_finished());
        let reserved: BTreeSet<_> = fleet.mgr("registrar")?.freeze_name_registrar()?.into_iter().collect();
        anyhow::ensure!(!reserved.contains(session));
        let digest = [17;32];
        let voters: Vec<_> = ["a","b","c"].into_iter().map(str::to_owned).collect();
        // C will receive the sealed log before receiving its signed directory.
        for name in ["a","b"] { fleet.mgr(name)?.install_name_charter(digest, voters.clone(), reserved.clone())?; }
        while !opening.is_finished() && Instant::now() < until { fleet.settle_except(&["registrar"])?; }
        let status = opening.join().map_err(|_| anyhow::anyhow!("founder stopped"))??;
        anyhow::ensure!(status.founded && status.owner.as_deref() == Some("a"));
        fleet.mgr("c")?.install_name_charter(digest, voters, reserved)?;
        fleet.settle_except(&["registrar"])?;
        anyhow::ensure!(fleet.mgr("c")?.status(session).is_some_and(|st| st.founded));
        for name in ["a","b","c"] {
            anyhow::ensure!(fleet.store(name)?.kept(session, 3)?.is_some_and(|s| s.events.len() == 1 && s.events.iter().all(|e| e.text == "original prompt")));
        }
        Ok(())
    }

    #[test]
    fn cp_t32_registrar_freeze_pins_the_origin_of_an_unvoted_wal_slot() -> anyhow::Result<()> {
        let fleet = Fleet::new(&["original", "registrar"])?;
        for (m,_,_,_) in fleet.nodes.values() { m.set_name_registrar("registrar")?; }
        let selected = fleet.mgr("original")?.clone();
        let session = "opening-at-cutoff";
        let opening = thread::spawn(move || selected.open_leader(session, 1, vec![user_event(session, 1, "selected")]));
        let deadline = Instant::now() + TEST_WAIT;
        while fleet.store("original")?.kept(session, 1)?.is_none_or(|s| s.events.is_empty()) && Instant::now() < deadline {
            anyhow::ensure!(fleet.mgr("original")?.flush(TEST_WAIT));
        }
        let original_status = fleet.mgr("original")?.status(session).context("opening local slot")?;
        let registrar = fleet.mgr("registrar")?;
        let mut beat = hyper_raft::proto::message(node_id("registrar"), MessageType::MsgHeartbeat);
        beat.from = node_id("original");
        beat.term = original_status.term;
        registrar.step("original", session, &beat.encode_to_vec())?;
        anyhow::ensure!(registrar.flush(TEST_WAIT));
        anyhow::ensure!(registrar.historical_name_inventory()?.iter().any(|s| s == session));
        let reserved: BTreeSet<_> = registrar.freeze_name_registrar()?.into_iter().collect();
        let digest = [2;32];
        let voters: Vec<_> = ["a","b","c"].into_iter().map(str::to_owned).collect();
        for (m,_,_,_) in fleet.nodes.values() { m.install_name_charter(digest, voters.clone(), reserved.clone())?; }
        let pinned: NamePin = serde_json::from_slice(&std::fs::read(fleet.wal_dir("registrar")?.join("name-admission.json"))?)?;
        anyhow::ensure!(pinned.pending.get(session).is_some_and(|p| p.selected == "original" && p.digest.is_none()));
        while !opening.is_finished() && Instant::now() < deadline { fleet.settle()?; }
        anyhow::ensure!(opening.join().map_err(|_| anyhow::anyhow!("original founder stopped"))??.founded);
        anyhow::ensure!(fleet.store("registrar")?.kept(session, 1)?.is_some_and(|s| s.events.first().is_some_and(|e| e.text == "selected")));
        Ok(())
    }

    #[test]
    fn cp_t32_conflicting_founders_cannot_publish_the_same_session_id() -> anyhow::Result<()> {
        if std::env::var_os("KILN_TEST_RAFT_REPAIR").is_some() {
            let _ = tracing_subscriber::fmt().with_env_filter("kiln::convo::raft=debug").with_test_writer().try_init();
        }
        let fleet = Fleet::new(&["a", "b", "c"])?;
        let session = "same-explicit-id";
        let run = |name: &'static str| -> anyhow::Result<_> {
            let mgr = fleet.mgr(name)?.clone();
            Ok(thread::spawn(move || mgr.open_leader(session, 1, vec![user_event(session, 1, name)])))
        };
        let first = run("a")?;
        let other = run("b")?;
        let until = Instant::now() + TEST_WAIT;
        // Let both selected candidates commit their independent index/term locally.
        for node in ["a","b"] {
            while fleet.store(node)?.kept(session, 8)?.is_none_or(|s| s.events.is_empty()) && Instant::now() < until { anyhow::ensure!(fleet.mgr(node)?.flush(TEST_WAIT)); }
            anyhow::ensure!(fleet.mgr(node)?.status(session).is_some_and(|st| !st.founded));
        }
        while !first.is_finished() && Instant::now() < until { fleet.settle_except(&["b"])?; }
        first.join().map_err(|_| anyhow::anyhow!("first founder stopped"))??;
        fleet.settle()?;
        anyhow::ensure!(other.join().map_err(|_| anyhow::anyhow!("other founder stopped"))?.is_err());
        let until = Instant::now() + TEST_WAIT;
        while !fleet.mgr("b")?.status(session).is_some_and(|s| s.owner.as_deref() == Some("a")) && Instant::now() < until { fleet.settle()?; }
        let loser = fleet.mgr("b")?.status(session);
        anyhow::ensure!(loser.as_ref().is_some_and(|s| s.founded && s.owner.as_deref() == Some("a")), "rejected founder failed to reconcile: {loser:?}, winner {:?}", fleet.mgr("a")?.status(session));
        anyhow::ensure!(fleet.store("b")?.kept(session, 8)?.is_some_and(|s| s.events.len() == 1 && s.events.iter().all(|e| e.text == "a")));
        anyhow::ensure!(fleet.store("c")?.kept(session, 8)?.is_some_and(|s| s.events.iter().all(|e| e.text == "a")));
        let winner = fleet.mgr("a")?.clone();
        let sending = thread::spawn(move || winner.propose_sync(session, 1, vec![user_event(session, 2, "after repair")]));
        let until = Instant::now() + TEST_WAIT;
        while !sending.is_finished() && Instant::now() < until { fleet.settle()?; }
        sending.join().map_err(|_| anyhow::anyhow!("winner stopped during follow-up"))??;
        fleet.settle()?;
        anyhow::ensure!(fleet.store("b")?.kept(session, 8)?.is_some_and(|s|
            s.events.iter().map(|e| e.text.as_str()).eq(["a", "after repair"])), "repaired follower did not apply the winner's next entry");
        Ok(())
    }

    #[test]
    fn cp_t32_three_distinct_origins_report_conflict_when_all_name_votes_split() -> anyhow::Result<()> {
        let fleet = Fleet::new(&["a","b","c"])?;
        let session = "three-simultaneous-origins";
        let voters: Vec<_> = ["a","b","c"].into_iter().map(str::to_owned).collect();
        let mut opens = Vec::new();
        for (name, (manager, _, _, _)) in &fleet.nodes {
            manager.set_name_registrar("registrar")?;
            manager.install_name_charter([6;32], voters.clone(), BTreeSet::new())?;
            let (m, name) = (manager.clone(), name.clone());
            opens.push(thread::spawn(move || m.open_leader(session, 1, vec![user_event(session, 1, &name)])));
        }
        let deadline = Instant::now() + TEST_WAIT;
        for (manager, store, _, _) in fleet.nodes.values() {
            while store.kept(session, 1)?.is_none_or(|k| k.events.is_empty()) && Instant::now() < deadline { anyhow::ensure!(manager.flush(TEST_WAIT)); }
        }
        while opens.iter().any(|op| !op.is_finished()) && Instant::now() < deadline { fleet.settle()?; }
        for opening in opens {
            let answer = opening.join().map_err(|_| anyhow::anyhow!("founder stopped"))?;
            anyhow::ensure!(answer.as_ref().is_err_and(|e| e.to_string().contains("conflicting starts")), "unexpected result {answer:?}");
        }
        for (manager, _, _, _) in fleet.nodes.values() {
            anyhow::ensure!(manager.status(session).is_some_and(|s| !s.founded) && manager.admission.lock().queued_bytes == 0);
        }
        Ok(())
    }

    #[test]
    fn cp_t32_replaced_database_with_same_wal_point_and_different_genesis_is_rebuilt() -> anyhow::Result<()> {
        let (_dir_a, db_a) = crate::convo::store::scratch()?;
        let (_dir_b, db_b) = crate::convo::store::scratch()?;
        let wal_a = tempfile::tempdir()?;
        let wal_b = tempfile::tempdir()?;
        let session = "same-index-and-term";
        let a = Manager::open("a", wal_a.path(), Some(db_a.clone()))?;
        let b = Manager::open("b", wal_b.path(), Some(db_b.clone()))?;
        a.open_leader(session, 1, vec![user_event(session, 1, "A")])?;
        b.open_leader(session, 1, vec![user_event(session, 1, "B")])?;
        a.shutdown();
        b.shutdown();
        anyhow::ensure!(db_a.raft_projection_frontier(session)? == db_b.raft_projection_frontier(session)?);
        anyhow::ensure!(db_a.raft_projection_origin(session)? != db_b.raft_projection_origin(session)?);
        let recovered = Manager::open("a", wal_a.path(), Some(db_b.clone()))?;
        anyhow::ensure!(recovered.status(session).is_some_and(|s| s.founded));
        anyhow::ensure!(db_b.raft_projection_origin(session)? == db_a.raft_projection_origin(session)?);
        anyhow::ensure!(db_b.kept(session, 2)?.is_some_and(|s| s.events.len() == 1 && s.events.iter().all(|e| e.text == "A")));
        Ok(())
    }

    #[test]
    fn cp_t32_historical_guard_survives_restart_while_established_session_runs() -> anyhow::Result<()> {
        let (_db, db) = crate::convo::store::scratch()?;
        let wal = tempfile::tempdir()?;
        let m = Manager::open_for_host("registrar", wal.path(), Some(db.clone()), Policy::default(), true)?;
        m.set_name_registrar("registrar")?;
        m.open_leader("known", 1, Vec::new())?;
        let previous = Arc::new(BTreeSet::from(["old-name".to_owned()]));
        m.guard_legacy_names(true, previous.clone())?;
        anyhow::ensure!(m.open_leader("unknown", 1, Vec::new()).is_err() && m.freeze_name_registrar().is_err());
        m.propose_sync("known", 1, vec![user_event("known", 2, "quorum established")])?;
        m.guard_legacy_names(false, previous.clone())?;
        anyhow::ensure!(m.open_leader("old-name", 1, Vec::new()).is_err());
        m.open_leader("available", 1, Vec::new())?;
        let names: BTreeSet<_> = m.freeze_name_registrar()?.into_iter().collect();
        m.install_name_charter([11;32], ["a","b","c"].into_iter().map(str::to_owned).collect(), names)?;
        m.shutdown();
        let reopened = Manager::open_for_host("registrar", wal.path(), Some(db), Policy::default(), true)?;
        anyhow::ensure!(reopened.name_charter_digest().is_none() && reopened.status("known").is_some_and(|s| s.founded));
        anyhow::ensure!(reopened.open_leader("new-without-cache", 1, Vec::new()).is_err());
        Ok(())
    }

    #[test]
    fn cp_t32_founder_and_first_message_wait_for_remote_durable_commit() -> anyhow::Result<()> {
        let fleet = Fleet::new(&["a", "b"])?;
        let source = fleet.mgr("a")?.clone();
        let session = "sess-bootstrap-receipt";
        let opening = thread::spawn(move || source.open_leader(session, 1, vec![user_event(session, 1, "open")]));
        let deadline = Instant::now() + TEST_WAIT;
        while fleet.store("a")?.kept(session, 10)?.is_none_or(|s| s.events.is_empty()) && Instant::now() < deadline {
            anyhow::ensure!(fleet.mgr("a")?.flush(TEST_WAIT));
        }
        anyhow::ensure!(fleet.mgr("a")?.status(session).is_some_and(|st| !st.founded) && !opening.is_finished(), "local apply alone exposed or acknowledged founding");
        while !opening.is_finished() && Instant::now() < deadline { fleet.settle()?; }
        opening.join().map_err(|_| anyhow::anyhow!("founder stopped"))??;
        let source = fleet.mgr("a")?.clone();
        let sending = thread::spawn(move || source.propose_sync(session, 1, vec![user_event(session, 2, "first")]));
        let deadline = Instant::now() + TEST_WAIT;
        while fleet.store("a")?.kept(session, 10)?.is_none_or(|s| s.events.len() < 2) && Instant::now() < deadline {
            anyhow::ensure!(fleet.mgr("a")?.flush(TEST_WAIT));
        }
        anyhow::ensure!(!sending.is_finished(), "local apply alone acknowledged the first message");
        while !sending.is_finished() && Instant::now() < deadline { fleet.settle()?; }
        sending.join().map_err(|_| anyhow::anyhow!("sender stopped"))??;
        anyhow::ensure!(fleet.store("b")?.kept(session, 10)?.is_some_and(|s| s.events.iter().any(|e| e.text == "first")));
        Ok(())
    }

    #[test]
    fn cp_t32_two_charter_voters_can_open_a_session_without_the_third_or_the_registrar() -> anyhow::Result<()> {
        let voters = ["a", "b", "c"];
        let fleet = Fleet::new_with_policy(&voters, Policy { auto_failover: true, ..Policy::default() })?;
        for (mgr, _, _, _) in fleet.nodes.values() {
            mgr.set_name_registrar("bootstrap")?;
            mgr.install_name_charter([7; 32], voters.iter().map(|v| (*v).to_owned()).collect(), BTreeSet::new())?;
            mgr.peer_unreachable("b");
        }
        let session = "two-charter-voters";
        let selected = fleet.mgr("c")?;
        let status = thread::scope(|scope| -> anyhow::Result<GroupStatus> {
            let opening = scope.spawn(|| selected.open_leader(session, 0, vec![user_event(session, 1, "offline opening")]));
            let deadline = Instant::now() + TEST_WAIT + (TEST_WAIT / 2);
            while !opening.is_finished() && Instant::now() < deadline { fleet.settle_except(&["b"])?; }
            opening.join().map_err(|_| anyhow::anyhow!("founder stopped"))?
        })?;
        anyhow::ensure!(status.founded && status.is_leader && status.owner.as_deref() == Some("c") && status.voters.len() == 3, "fresh group={status:?}");
        fleet.settle_except(&["b"])?;
        anyhow::ensure!(fleet.store("a")?.kept(session, 1)?.is_some_and(|s| s.events.iter().any(|e| e.text == "offline opening")));
        anyhow::ensure!(fleet.mgr("b")?.status(session).is_none(), "offline node received session traffic");
        Ok(())
    }

    #[test]
    fn cp_t32_third_executor_enrolled_after_founding_joins_the_session_quorum() -> anyhow::Result<()> {
        let mut fleet = Fleet::new_with_policy(&["a","b"], Policy { auto_failover: true, ..Policy::default() })?;
        let session = "late-voter";
        fleet.open("b", session, 1, vec![user_event(session, 1, "on b")])?;
        let before = fleet.mgr("b")?.status(session).context("missing founding group")?;
        anyhow::ensure!(before.voters.len() == 1 && before.owner.as_deref() == Some("b"));
        fleet.add_node("c")?;
        let until = Instant::now() + TEST_WAIT;
        while fleet.mgr("b")?.status(session).is_none_or(|s| s.voters.len() != 3) && Instant::now() < until { fleet.settle()?; }
        fleet.settle()?;
        for name in ["a","b","c"] {
            let st = fleet.mgr(name)?.status(session).context("new voter has no session")?;
            anyhow::ensure!(st.founded && st.voters.len() == 3 && st.owner.as_deref() == Some("b"), "late voter {name} has {st:?}");
        }
        anyhow::ensure!(fleet.store("c")?.kept(session, 8)?.is_some_and(|s| s.events.len() == 1));
        Ok(())
    }

    #[test]
    fn cp_t32_storage_only_members_replicate_without_becoming_executor_voters() -> anyhow::Result<()> {
        let fleet = Fleet::new_with_policy(&["owner", "replica", "worker"], Policy { auto_failover: true, ..Policy::default() })?;
        for (mgr, _, _, _) in fleet.nodes.values() { mgr.set_peer_executor("replica", false); }
        fleet.open("owner", "sess-storage-vote", 1, vec![user_event("sess-storage-vote", 1, "replicated")])?;
        fleet.settle()?;
        let status = fleet.mgr("owner")?.status("sess-storage-vote").context("missing group")?;
        anyhow::ensure!(status.voters == [node_id("owner")] && status.learners.contains(&node_id("replica")));
        anyhow::ensure!(!fleet.mgr("owner")?.transfer_at("sess-storage-vote", "replica", 1));
        anyhow::ensure!(fleet.store("replica")?.kept("sess-storage-vote", 10)?.is_some_and(|s| s.events.len() == 1));
        Ok(())
    }

    #[test]
    fn cp_t32_in_place_recovery_keeps_elected_node_and_claims_a_new_epoch() -> anyhow::Result<()> {
        let fleet = Fleet::new(&["a"])?;
        let session = "resume-in-place";
        fleet.open("a", session, 1, vec![user_event(session, 1, "resume here")])?;
        let manager = fleet.mgr("a")?;
        let digest = "e".repeat(64);
        fleet.while_pumping(|| manager.move_to(session, "a", 1, digest.clone()))?;
        fleet.settle()?;
        let status = manager.status(session).context("missing in-place recovery")?;
        anyhow::ensure!(status.is_leader && status.epoch == 2 && !status.native_move_ready);
        fleet.while_pumping(|| manager.confirm_native_activation(session, 2, digest.clone()))?;
        let manager = manager.clone();
        crate::rt::block_on(async move { manager.wait_moved(session, "a", 1, &digest).await })
    }

    #[test]
    fn cp_t32_a_disk_handoff_and_one_bad_path_cannot_transfer_the_providers_epoch() -> anyhow::Result<()> {
        use hyper_timing::Trust;
        let fleet=Fleet::new_with_policy(&["a","b","c"],Policy{auto_failover:true, ..Policy::default()})?;
        let session="bad-path-during-disk-handoff";
        let (a,b,c)=(fleet.mgr("a")?,fleet.mgr("b")?,fleet.mgr("c")?);
        let opened=fleet.open("b",session,1,vec![user_event(session,1,"original")])?;
        fleet.settle()?;
        fleet.detector_partition(&[],false)?;
        let broken=[(b.my_id,a.my_id)];
        fleet.detect_links(&[],false,&broken,|| a.live.with(|l|l.trust(b.my_id))==Some(Some(Trust::Suspected))
            && c.live.with(|l|matches!(l.trust(b.my_id),Some(Trust::Trusted{..})))==Some(true))?;
        b.live.report_io(0,None,Some(b.live.now_ns()));
        fleet.detect_links(&[],false,&broken,|| b.live.with(|l|l.own_disk_slow())==Some(true) && c.live.with(|l|l.disk_slow(b.my_id))==Some(Some(true)))?;
        let until=Instant::now()+TEST_WAIT;
        loop {
            fleet.settle()?;
            let (tx,rx)=mpsc::channel();
            a.tx.send(Input::ProcessRefusal(session.into(),c.my_id,tx)).map_err(|_|anyhow::anyhow!("A stopped"))?;
            if rx.recv_timeout(TEST_WAIT)? {break;}
            anyhow::ensure!(Instant::now()<until,"C did not attest its continuing trust of B: A {:?}, B {:?}, C {:?}",a.status(session),b.status(session),c.status(session));
        }
        let incumbent=b.status(session).context("missing incumbent")?;
        anyhow::ensure!(a.status(session).is_some_and(|s|s.is_leader && s.epoch==opened.epoch));
        anyhow::ensure!(incumbent.execution_held && incumbent.owner.as_deref()==Some("b") && incumbent.epoch==opened.epoch,"{incumbent:?}");
        anyhow::ensure!(a.owned_epoch(session).is_none());
        // Losing B itself changes C's verdict. Its parked watch must wake the new
        // leader, which still needs a fresh majority before the held execution can move.
        b.live.report_io(0,None,None);
        fleet.elect_survivor(&["b"],session)?;
        anyhow::ensure!([a,c].iter().any(|m|m.owned_epoch(session)==opened.epoch.checked_add(1)));
        Ok(())
    }

    #[test]
    fn cp_t32_a_disk_handoff_releases_no_provider_or_receipt_and_restores_its_epoch() -> anyhow::Result<()> {
        let fleet = Fleet::new_with_policy(&["a","b","c"],Policy { auto_failover: true, ..Policy::default() })?;
        let session = "disk-handoff";
        let owner = fleet.mgr("a")?;
        let opened = fleet.open("a", session, 1, vec![user_event(session,1,"prior receipt")])?;
        fleet.settle()?;
        fleet.detector_partition(&[],false)?;
        let peers = [fleet.mgr("b")?, fleet.mgr("c")?];
        let disk_observed = |slow| {
            owner.live.with(|l| l.own_disk_slow()) == Some(slow)
                && peers.iter().all(|peer| peer.live.with(|l| l.disk_slow(owner.my_id)) == Some(Some(slow)))
        };
        owner.live.report_io(0, None, Some(owner.live.now_ns()));
        fleet.detect_until(&[], false, || disk_observed(true))?;
        fleet.settle()?;
        let old = owner.status(session).context("missing original owner")?;
        anyhow::ensure!(old.execution_held && !old.is_leader && old.owner.as_deref()==Some("a") && old.epoch==opened.epoch, "{old:?}");
        anyhow::ensure!(owner.owned_epoch(session).is_none());
        anyhow::ensure!(owner.propose_sync(session,opened.epoch,vec![user_event(session,2,"held")]).is_err());
        for other in ["b","c"] {
            let ledger = fleet.mgr(other)?.status(session).context("missing held replica")?;
            anyhow::ensure!(ledger.execution_held && ledger.owner.as_deref()==Some("a") && ledger.epoch==opened.epoch, "{ledger:?}");
        }
        owner.live.report_io(0, None, None);
        fleet.detect_until(&[], false, || disk_observed(false))?;
        fleet.settle()?;
        let restored = owner.status(session).context("missing restored owner")?;
        anyhow::ensure!(restored.is_leader && !restored.execution_held && owner.owned_epoch(session)==Some(opened.epoch), "{restored:?}");
        Ok(())
    }

    #[test]
    fn cp_t32_native_move_waits_for_the_elected_destinations_activation_receipt() -> anyhow::Result<()> {
        let fleet = Fleet::new(&["a", "b", "c"])?;
        let session = "sess-native-move";
        fleet.open("a", session, 1, vec![user_event(session, 1, "ready")])?;
        fleet.settle()?;
        let digest = "a".repeat(64);
        fleet.detector_partition(&[],false)?;
        fleet.while_pumping(|| fleet.mgr("a")?.move_to(session, "b", 1, digest.clone()))?;
        fleet.settle()?;
        let target = fleet.mgr("b")?.status(session).context("destination session")?;
        anyhow::ensure!(target.is_leader && target.epoch == 2 && target.native_move_digest == Some(digest.clone()) && !target.native_move_ready);
        let source = fleet.mgr("a")?.clone();
        let expected = digest.clone();
        let waiting = thread::spawn(move || crate::rt::block_on(async move { source.wait_moved(session, "b", 1, &expected).await }));
        let b = fleet.mgr("b")?.clone();
        let acknowledge = thread::spawn(move || -> anyhow::Result<()> {
            anyhow::ensure!(b.confirm_native_activation(session, 2, "b".repeat(64)).is_err(), "wrong native digest cannot open custody");
            b.confirm_native_activation(session, 2, digest)
        });
        let until = Instant::now() + TEST_WAIT;
        while !acknowledge.is_finished() && Instant::now() < until { fleet.settle()?; }
        acknowledge.join().map_err(|_| anyhow::anyhow!("destination receipt task stopped"))??;
        fleet.settle()?;
        waiting.join().map_err(|_| anyhow::anyhow!("source move waiter stopped"))??;
        Ok(())
    }

    /// CP-T3.2. The old leader gives a hand-over up once its target can't be reached and commits
    /// that it did: the source's wait ends on that word, told rather than timed, and every member
    /// of the session learns it.
    #[test]
    fn cp_t32_the_old_leader_commits_giving_a_hand_over_up_and_its_waiter_is_told() -> anyhow::Result<()> {
        let fleet = Fleet::new(&["a", "b", "c"])?;
        let session = "sess-given-up";
        fleet.open("a", session, 1, vec![user_event(session, 1, "ready")])?;
        fleet.settle()?;
        // Measured links: the leader beats, so its followers learn each commit.
        fleet.detector_partition(&[], false)?;
        let digest = "a".repeat(64);
        let intent = MoveIntent { leadership_only: false, cancel: false, ready: false, epoch: 1, target: "b".into(), digest: digest.clone() };
        fleet.mgr("a")?.peer_unreachable("b");
        fleet.while_pumping(|| fleet.mgr("a")?.commit_move_intent(session, 1, intent))?;
        let source = fleet.mgr("a")?.clone();
        let expected = digest.clone();
        let waiting = thread::spawn(move || crate::rt::block_on(async move { source.wait_moved(session, "b", 1, &expected).await }));
        let until = Instant::now() + TEST_WAIT;
        while !waiting.is_finished() && Instant::now() < until { fleet.settle()?; }
        anyhow::ensure!(waiting.is_finished(), "the source's wait must end on its committed word");
        let told = waiting.join().map_err(|_| anyhow::anyhow!("source move waiter stopped"))?;
        anyhow::ensure!(told.as_ref().is_err_and(|e| format!("{e:#}").contains("gave the hand-over up")), "the wait must say the hand-over was given up: {told:?}");
        let member = fleet.mgr("c")?.status(session).context("member status")?;
        anyhow::ensure!(member.abandoned_move == Some(AbandonedMove { epoch: 1, target: "b".into(), digest }), "every member learns it: {member:?}");
        Ok(())
    }

    /// An open waits on what decides it, never a timer: once the one member that could hold its
    /// copy is unreachable, it's answered at once that no member remains to hold it.
    #[test]
    fn cp_t32_an_open_is_answered_once_no_member_remains_to_hold_its_copy() -> anyhow::Result<()> {
        let fleet = Fleet::new(&["a", "b"])?;
        let session = "sess-no-copy";
        let opener = fleet.mgr("a")?.clone();
        let opening = thread::spawn(move || opener.open_leader(session, 1, vec![user_event(session, 1, "first")]));
        fleet.mgr("a")?.peer_unreachable("b");
        let until = Instant::now() + TEST_WAIT;
        while !opening.is_finished() && Instant::now() < until {
            fleet.mgr("a")?.drive_due();
            thread::sleep(TEST_POLL);
        }
        anyhow::ensure!(opening.is_finished(), "the open must be answered on the event");
        let answer = opening.join().map_err(|_| anyhow::anyhow!("opener stopped"))?;
        anyhow::ensure!(answer.as_ref().is_err_and(|e| format!("{e:#}").contains("no member remains to hold its copy")), "{answer:?}");
        Ok(())
    }

    /// CP-T3.2. A session's group promotes to 2f + 1 voters of its configured f, at least 3, as a
    /// ZooKeeper ensemble is sized; a policy written before `faults` existed reads as f = 1, and an
    /// unset f writes nothing new.
    #[test]
    fn cp_t32_voters_are_two_f_plus_one_of_the_configured_faults() -> anyhow::Result<()> {
        anyhow::ensure!(Policy::default().voters() == 3);
        anyhow::ensure!(Policy { auto_failover: true, faults: Some(2) }.voters() == 5);
        anyhow::ensure!(Policy { auto_failover: true, faults: Some(0) }.voters() == MIN_VOTERS);
        let older: Policy = serde_json::from_str(r#"{"auto_failover":true}"#)?;
        anyhow::ensure!(older.faults.is_none() && older.voters() == 3);
        anyhow::ensure!(serde_json::to_string(&Policy { auto_failover: true, faults: None })? == r#"{"auto_failover":true}"#);
        Ok(())
    }

    #[test]
    fn leadership_transfers_when_session_moves_to_another_runner() -> anyhow::Result<()> {
        let fleet = Fleet::new(&["laptop-a", "laptop-b", "laptop-c"])?;
        fleet.open("laptop-b", "sess-move", 1, vec![user_event("sess-move", 1, "before move")])?;
        fleet.settle()?;
        anyhow::ensure!(fleet.mgr("laptop-a")?.leader_of("sess-move").as_deref() == Some("laptop-b"));

        fleet.detector_partition(&[],false)?;
        anyhow::ensure!(fleet.mgr("laptop-b")?.transfer_leadership("sess-move", "laptop-c"));
        fleet.settle()?;

        let c_status = fleet.mgr("laptop-c")?.status("sess-move").ok_or_else(|| anyhow::anyhow!("laptop-c missing group"))?;
        anyhow::ensure!(c_status.is_leader && c_status.leader_name.as_deref() == Some("laptop-c"), "laptop-c must lead after transfer: {c_status:?}");
        fleet.mgr("laptop-c")?.propose("sess-move", 2, vec![user_event("sess-move", 2, "after move")])?;
        fleet.settle()?;

        fleet.store("laptop-a")?.sync()?;
        let kept = fleet.store("laptop-a")?.kept("sess-move", 10)?.ok_or_else(|| anyhow::anyhow!("laptop-a missing moved session"))?;
        anyhow::ensure!(kept.events.iter().any(|e| e.text == "after move"), "new leader's write must replicate to laptop-a: {:?}", kept.events);
        Ok(())
    }

    /// Exercises the exact production restart & WAL replay path against a REAL Turso `Store`:
    /// entries committed to `convo-raft-000.log` survive shutdown (joining the owner thread) and
    /// replay idempotently into a backing `Store` on reopen even if `Store` lagged behind the WAL.
    #[test]
    fn wal_persists_entries_and_recovers_into_real_store_after_node_restart() -> anyhow::Result<()> {
        let wal_dir = tempfile::tempdir()?;
        let wal_path = shard_path(wal_dir.path(), 0);
        let (_sdir1, store_1) = crate::convo::store::scratch()?;

        let committed_before = {
            let mgr = Manager::open("laptop-b", wal_dir.path(), Some(store_1.clone()))?;
            let st = mgr.open_leader("sess-wal", 3, vec![user_event("sess-wal", 1, "persisted-1")])?;
            anyhow::ensure!(st.is_leader && st.applied >= 2, "initial entry must commit to WAL: {st:?}");
            mgr.propose("sess-wal", 3, vec![user_event("sess-wal", 2, "persisted-2")])?;
            anyhow::ensure!(mgr.flush(TEST_WAIT), "WAL flush timed out");
            let st2 = mgr.status("sess-wal").ok_or_else(|| anyhow::anyhow!("missing status before restart"))?;
            anyhow::ensure!(st2.applied > st.applied, "second entry must advance applied in WAL: {st2:?}");
            anyhow::ensure!(wal_path.exists() && std::fs::metadata(&wal_path)?.len() > 0, "convo-raft-000.log must exist on disk");
            let applied = st2.applied;
            mgr.shutdown();
            anyhow::ensure!(mgr.thread.lock().is_none(), "owner thread must be joined and closed before restart");
            applied
        };

        // Reopen against a fresh Turso Store (proving WAL replay `replaying = true` restores every
        // committed row directly into Store without any test-only `mem_rows` path!).
        let (_sdir2, store_2) = crate::convo::store::scratch()?;
        let mgr_after = Manager::open("laptop-b", wal_dir.path(), Some(store_2.clone()))?;
        anyhow::ensure!(mgr_after.flush(TEST_WAIT), "WAL reopen flush timed out");
        store_2.sync()?;
        let st_after = mgr_after.status("sess-wal").ok_or_else(|| anyhow::anyhow!("sess-wal missing after WAL restart"))?;
        anyhow::ensure!(st_after.is_leader && st_after.applied >= committed_before, "recovered leader must preserve WAL index (before={committed_before}): {st_after:?}");
        anyhow::ensure!(st_after.epoch == 4, "re-elected owner must fence the prior process epoch: {st_after:?}");
        let kept = store_2.kept("sess-wal", 10)?.ok_or_else(|| anyhow::anyhow!("Store missing WAL-replayed session"))?;
        anyhow::ensure!(kept.events.len() == 2, "both WAL-committed entries must replay into real Store: {:?}", kept.events);
        Ok(())
    }

    /// Verifies that an off-thread, wire-chunked full-history snapshot captures the EXACT committed
    /// state at `applied` (`seq == 0..=320`, all `Row::Record` lines + byte mark, all `Row::Inbox`
    /// rows), NEVER leaks uncommitted future rows written ahead to the leader's `Store`, and splits
    /// cleanly across `KRC1` wire chunks (`<= sync::wire::PART`).
    #[test]
    fn lagging_peer_catches_up_full_history_via_chunked_off_thread_snapshot_after_compaction() -> anyhow::Result<()> {
        let mut fleet = Fleet::new(&["laptop-b"])?;
        let total_events: u64 = 320;
        let open_row = Row::Session {
            session: "sess-hist".into(),
            info: Some(r#"{"id":"sess-hist","target":"agent:laptop-b"}"#.into()),
            meta: None,
            state: None,
            changes: None,
            epoch: Some(1),
            open: true,
            at: 1000,
        };
        let record_rows = vec![
            Row::Record(Box::new(RecordRow {
                session: "sess-hist".into(),
                source: "claude-transcript-1".into(),
                n: 0,
                end: 42,
                line: r#"{"type":"user","text":"first line"}"#.into(),
                kept: None,
            })),
            Row::Record(Box::new(RecordRow {
                session: "sess-hist".into(),
                source: "claude-transcript-1".into(),
                n: 1,
                end: 98,
                line: r#"{"type":"assistant","text":"second line"}"#.into(),
                kept: None,
            })),
        ];
        let inbox_rows = vec![
            Row::Inbox(Box::new(InboxRow {
                session: "sess-hist".into(),
                family: "inbox-heads".into(),
                key: b"sess-hist".to_vec(),
                value: Some(br#"{"epoch":1,"moved":false}"#.to_vec()),
            })),
            Row::Inbox(Box::new(InboxRow {
                session: "sess-hist".into(),
                family: "inbox".into(),
                key: b"sess-hist/msg-id-1".to_vec(),
                value: Some(br#"{"epoch":1,"digest":"abc"}"#.to_vec()),
            })),
        ];
        let mut initial = vec![open_row];
        initial.extend(record_rows);
        initial.extend(inbox_rows);
        fleet.store("laptop-b")?.put(initial.clone());
        fleet.open("laptop-b", "sess-hist", 1, initial)?;

        // Write seq == 0..=320 with ~2 KiB payloads so the full snapshot exceeds WIRE_PART_MAX
        // (512 KiB) and exercises multi-frame `KRC1` chunked MsgSnapshot transport!
        let pad = "p".repeat(1800);
        for seq in 0..=total_events {
            let ev = user_event("sess-hist", seq, &format!("msg-{seq}-{pad}"));
            fleet.store("laptop-b")?.put(vec![ev.clone()]);
            fleet.mgr("laptop-b")?.propose("sess-hist", 1, vec![ev])?;
        }
        fleet.settle()?;

        // Simulate the harness tee writing an UNCOMMITTED future event + Closed row directly into
        // the leader's Store ahead of Raft: the snapshot taken at `applied` MUST NOT leak them!
        fleet.store("laptop-b")?.put(vec![
            user_event("sess-hist", 999, "uncommitted-future-row"),
            Row::Closed { session: "sess-hist".into(), at: 9999 },
        ]);
        fleet.store("laptop-b")?.sync()?;

        // Compact off the owner thread with `keep = 0` to force a full snapshot for the late peer.
        fleet.mgr("laptop-b")?.force_compact("sess-hist", 0)?;
        fleet.settle()?;

        fleet.add_node("laptop-late")?;
        let before = fleet.snapshot_bytes.load(Ordering::Relaxed);
        let measured = crate::os::counted::ProcessWindow::begin();
        let began = Instant::now();
        fleet.settle()?;
        let sent = fleet.snapshot_bytes.load(Ordering::Relaxed).saturating_sub(before);
        if let Some(window) = measured { window.report("snapshot transfer"); }
        anyhow::ensure!(sent > WIRE_PART_MAX as u64);
        if std::env::var_os("KILN_TEST_TRANSFER_TIMING").is_some() {
            eprintln!("snapshot transfer: {sent} bytes, {} microseconds through fleet settlement", began.elapsed().as_micros());
        }
        fleet.store("laptop-late")?.sync()?;

        let wire_chunks = fleet.mgr("laptop-b")?.shared.lock().wire_chunks_sent;
        anyhow::ensure!(wire_chunks >= 2, "large MsgSnapshot must split into >= 2 KRC1 wire chunks, got {wire_chunks}");

        let kept = fleet.store("laptop-late")?.kept("sess-hist", 500)?.ok_or_else(|| anyhow::anyhow!("laptop-late missing snapshot session"))?;
        anyhow::ensure!(kept.events.len() == usize::try_from(total_events + 1)?, "late peer must receive EXACT committed 321 events (seq 0..={total_events}), got {}", kept.events.len());
        anyhow::ensure!(kept.events.first().is_some_and(|e| e.seq == 0 && e.text.starts_with("msg-0-")), "seq 0 event must be preserved");
        anyhow::ensure!(kept.events.last().is_some_and(|e| e.seq == total_events && e.text.starts_with(&format!("msg-{total_events}-"))), "latest committed event must be preserved");
        anyhow::ensure!(!kept.events.iter().any(|e| e.seq == 999), "uncommitted future Store row must NEVER leak into snapshot");
        anyhow::ensure!(kept.closed_at.is_none(), "uncommitted future Closed in Store must NEVER leak into snapshot");

        let marks = fleet.store("laptop-late")?.marks("sess-hist")?;
        anyhow::ensure!(marks.len() == 1 && marks.first().is_some_and(|m| m.source == "claude-transcript-1" && m.n == 1 && m.end == 98), "snapshot must restore record marks: {marks:?}");
        let rec_lines = fleet.store("laptop-late")?.record("sess-hist", "claude-transcript-1", 0, 10)?;
        anyhow::ensure!(rec_lines.len() == 2, "snapshot must restore all record lines: {rec_lines:?}");

        let heads = fleet.store("laptop-late")?.inbox_rows("inbox-heads", b"sess-hist".to_vec())?;
        anyhow::ensure!(heads.len() == 1 && heads.first().is_some_and(|(k, v, _)| k == b"sess-hist" && v == br#"{"epoch":1,"moved":false}"#), "snapshot must restore inbox-heads: {heads:?}");
        let msgs = fleet.store("laptop-late")?.inbox_rows("inbox", b"sess-hist/".to_vec())?;
        anyhow::ensure!(msgs.len() == 1 && msgs.first().is_some_and(|(k, _, _)| k == b"sess-hist/msg-id-1"), "snapshot must restore inbox messages: {msgs:?}");
        Ok(())
    }

    #[test]
    fn oversized_batch_splits_and_never_drops_rows() -> anyhow::Result<()> {
        let fleet = Fleet::new(&["laptop-a", "laptop-b"])?;
        fleet.open("laptop-b", "sess-big", 1, Vec::new())?;
        let big_text = "x".repeat(100 * 1024);
        let batch: Vec<Row> = (1..=12u64).map(|seq| user_event("sess-big", seq, &format!("{seq}-{big_text}"))).collect();
        fleet.mgr("laptop-b")?.propose("sess-big", 1, batch)?;
        fleet.settle()?;

        fleet.store("laptop-a")?.sync()?;
        let kept = fleet.store("laptop-a")?.kept("sess-big", 20)?.ok_or_else(|| anyhow::anyhow!("laptop-a missing split batch"))?;
        anyhow::ensure!(kept.events.len() == 12, "all 12 rows from oversized batch must replicate intact, got {}", kept.events.len());
        Ok(())
    }

    /// Verifies that a SINGLE row exceeding `ENTRY_DATA_MAX` (~764 KiB) — e.g. a 1.2 MiB tool
    /// output — chunks cleanly across `WireEntry::Part` WAL entries and replicates intact with 0 drops.
    #[test]
    fn oversized_single_row_chunks_through_wal_without_dropping() -> anyhow::Result<()> {
        let fleet = Fleet::new(&["laptop-a", "laptop-b"])?;
        fleet.open("laptop-b", "sess-huge-row", 1, Vec::new())?;
        let drops_before = dropped_proposals();
        let huge_payload = "Z".repeat(1200 * 1024);
        let huge_ev = user_event("sess-huge-row", 1, &huge_payload);
        anyhow::ensure!(serde_json::to_vec(&huge_ev)?.len() > ENTRY_DATA_MAX, "test row must exceed ENTRY_DATA_MAX");
        fleet.mgr("laptop-b")?.propose("sess-huge-row", 1, vec![huge_ev])?;
        fleet.settle()?;

        anyhow::ensure!(dropped_proposals() == drops_before, "oversized single row must never drop");
        fleet.store("laptop-a")?.sync()?;
        let kept = fleet.store("laptop-a")?.kept("sess-huge-row", 10)?.ok_or_else(|| anyhow::anyhow!("laptop-a missing chunked single row"))?;
        anyhow::ensure!(kept.events.len() == 1 && kept.events.first().is_some_and(|e| e.text.len() == huge_payload.len()), "chunked 1.2 MiB row must replicate intact");
        Ok(())
    }

    /// Verifies that exceeding one WAL shard's group limit automatically opens a new WAL shard
    /// without evicting any live led session, and recovers all shards into a real `Store` on restart.
    #[test]
    fn sharded_wal_grows_across_shards_without_evicting_active_sessions() -> anyhow::Result<()> {
        let wal_dir = tempfile::tempdir()?;
        let (_sdir1, store_1) = crate::convo::store::scratch()?;
        {
            let mgr = Manager::open_with_shard_groups("laptop-b", wal_dir.path(), Some(store_1), 2)?;
            for i in 1..=5u64 {
                let s = format!("sess-{i}");
                let st = mgr.open_leader(&s, 1, vec![user_event(&s, 1, &format!("work-{i}"))])?;
                anyhow::ensure!(st.is_leader && st.applied >= 2, "{s} must commit to WAL: {st:?}");
            }
            anyhow::ensure!(mgr.flush(TEST_WAIT));
            let shards = mgr.shared.lock().shard_count;
            anyhow::ensure!(shards >= 3, "5 live sessions across 2-group shards must span >= 3 WAL shards, got {shards}");
            anyhow::ensure!(mgr.all_status().len() == 5, "zero live sessions may be evicted");
            mgr.shutdown();
        }
        let (_sdir2, store_2) = crate::convo::store::scratch()?;
        let reopened = Manager::open_with_shard_groups("laptop-b", wal_dir.path(), Some(store_2.clone()), 2)?;
        anyhow::ensure!(reopened.flush(TEST_WAIT));
        store_2.sync()?;
        anyhow::ensure!(reopened.all_status().len() == 5, "all 5 sessions across 3 WAL shards must recover after restart");
        for i in 1..=5u64 {
            let s = format!("sess-{i}");
            let st = reopened.status(&s).ok_or_else(|| anyhow::anyhow!("missing {s} after multi-shard restart"))?;
            anyhow::ensure!(st.is_leader && st.epoch == 2, "{s} must commit a new custody epoch: {st:?}");
            anyhow::ensure!(store_2.kept(&s, 10)?.is_some_and(|k| k.events.len() == 1), "{s} must replay into real Store");
        }
        Ok(())
    }

    /// Verifies that `Manager::shutdown` drains already-queued proposals durably into the WAL before
    /// closing files, so reopening against a real `Store` recovers every queued proposal without loss.
    #[test]
    fn shutdown_drains_queued_proposals_durably_to_wal() -> anyhow::Result<()> {
        let wal_dir = tempfile::tempdir()?;
        let (_sdir1, store_1) = crate::convo::store::scratch()?;
        {
            let mgr = Manager::open("laptop-b", wal_dir.path(), Some(store_1))?;
            mgr.open_leader("sess-drain", 1, vec![user_event("sess-drain", 1, "init")])?;
            for seq in 2..=10u64 {
                mgr.propose("sess-drain", 1, vec![user_event("sess-drain", seq, &format!("queued-{seq}"))])?;
            }
            mgr.shutdown();
        }
        let (_sdir2, store_2) = crate::convo::store::scratch()?;
        let reopened = Manager::open("laptop-b", wal_dir.path(), Some(store_2.clone()))?;
        anyhow::ensure!(reopened.flush(TEST_WAIT));
        store_2.sync()?;
        let kept = store_2.kept("sess-drain", 20)?.ok_or_else(|| anyhow::anyhow!("missing sess-drain after restart"))?;
        anyhow::ensure!(kept.events.len() == 10, "all 10 queued rows must flush durably on shutdown and replay into Store, got {}", kept.events.len());
        Ok(())
    }

    /// Verifies `move_wal_dir` during blue/green handover move (including setting aside a stale
    /// destination directory) and rollback back to original `own_dir` against real `Store`s.
    #[test]
    fn wal_directory_handover_move_and_rollback_preserves_log() -> anyhow::Result<()> {
        let parent = tempfile::tempdir()?;
        let dir_old = parent.path().join("old-own/convo-raft");
        let dir_new = parent.path().join("new-own/convo-raft");
        std::fs::create_dir_all(parent.path().join("old-own"))?;
        // Pre-create a stale directory at `dir_new` to verify `move_wal_dir` sets it aside rather than skipping!
        std::fs::create_dir_all(&dir_new)?;
        let (_sdir1, store_1) = crate::convo::store::scratch()?;

        let applied_1 = {
            let mgr = Manager::open("laptop-b", &dir_old, Some(store_1))?;
            let st = mgr.open_leader("sess-bg", 1, vec![user_event("sess-bg", 1, "from-old")])?;
            anyhow::ensure!(mgr.flush(TEST_WAIT));
            let a = st.applied;
            mgr.shutdown();
            a
        };
        move_wal_dir(&dir_old, &dir_new)?;

        let (_sdir2, store_2) = crate::convo::store::scratch()?;
        let applied_2 = {
            let mgr = Manager::open("laptop-b", &dir_new, Some(store_2))?;
            anyhow::ensure!(mgr.flush(TEST_WAIT));
            let epoch = mgr.status("sess-bg").context("missing recovered custody")?.epoch;
            mgr.propose("sess-bg", epoch, vec![user_event("sess-bg", 2, "from-new")])?;
            anyhow::ensure!(mgr.flush(TEST_WAIT));
            let st = mgr.status("sess-bg").ok_or_else(|| anyhow::anyhow!("missing sess-bg in new build"))?;
            anyhow::ensure!(st.applied > applied_1, "incoming build must append onto existing log");
            let a = st.applied;
            mgr.shutdown();
            a
        };

        move_wal_dir(&dir_new, &dir_old)?;
        let (_sdir3, store_3) = crate::convo::store::scratch()?;
        let mgr_back = Manager::open("laptop-b", &dir_old, Some(store_3.clone()))?;
        anyhow::ensure!(mgr_back.flush(TEST_WAIT));
        store_3.sync()?;
        let st_back = mgr_back.status("sess-bg").ok_or_else(|| anyhow::anyhow!("missing sess-bg after rollback"))?;
        anyhow::ensure!(st_back.is_leader && st_back.applied >= applied_2, "rollback must preserve full log without reset: {st_back:?}");
        let kept = store_3.kept("sess-bg", 10)?.ok_or_else(|| anyhow::anyhow!("missing sess-bg rows in Store after rollback"))?;
        anyhow::ensure!(kept.events.len() == 2, "both entries must survive handover move + rollback, got {}", kept.events.len());
        Ok(())
    }

    /// Verifies `hyper-durable` fencing recovery WITHOUT sleeps:
    /// 1. A transient `Fatal` during `persist` (e.g. store sync failure) fences the replica;
    ///    `reopen_fenced` automatically reopens it from durable state, increments `reopened_replicas()`,
    ///    and the session genuinely RESUMES proposing and replicating new rows to followers!
    /// 2. A permanent corrupt entry increments `corrupt_entries()` and is held after its reopen
    ///    fails, until an event could change that, instead of spinning in a tight loop.
    #[test]
    fn fenced_replica_reopens_from_durable_state_and_resumes() -> anyhow::Result<()> {
        let fleet = Fleet::new(&["laptop-a", "laptop-b"])?;
        let open_row = Row::Session {
            session: "sess-fence".into(),
            info: Some(r#"{"id":"sess-fence","target":"agent:laptop-b"}"#.into()),
            meta: None,
            state: None,
            changes: None,
            epoch: Some(1),
            open: true,
            at: 1000,
        };
        fleet.store("laptop-b")?.put(vec![open_row.clone(), user_event("sess-fence", 1, "before-fence")]);
        fleet.open("laptop-b", "sess-fence", 1, vec![open_row, user_event("sess-fence", 1, "before-fence")])?;
        fleet.settle()?;

        // 1. Inject ONE transient persist fault -> compact triggers persist() -> Fatal fences replica
        // -> Owner automatically reopens from durable state -> session resumes and replicates new event!
        let reopened_before = reopened_replicas();
        fleet.mgr("laptop-b")?.fail_next_persist("sess-fence")?;
        let compact_err = fleet.mgr("laptop-b")?.force_compact("sess-fence", 0);
        anyhow::ensure!(compact_err.is_err(), "injected persist failure must fence compact: {compact_err:?}");
        // The error wakes the caller before the owner has finished reopening.
        fleet.settle()?;
        anyhow::ensure!(reopened_replicas() > reopened_before, "fenced replica must be reopened automatically");
        let after_ev = user_event("sess-fence", 2, "after-reopen-resumed");
        fleet.store("laptop-b")?.put(vec![after_ev.clone()]);
        let epoch = fleet.mgr("laptop-b")?.status("sess-fence").context("missing recovered custody")?.epoch;
        fleet.mgr("laptop-b")?.propose("sess-fence", epoch, vec![after_ev])?;
        fleet.settle()?;
        fleet.store("laptop-a")?.sync()?;
        let kept = fleet.store("laptop-a")?.kept("sess-fence", 10)?.ok_or_else(|| anyhow::anyhow!("laptop-a missing resumed session"))?;
        anyhow::ensure!(
            kept.events.iter().any(|e| e.text == "after-reopen-resumed"),
            "reopened replica must resume replicating new rows to follower: {:?}",
            kept.events
        );

        // 2. Permanent corrupt entry increments `corrupt_entries()` and is held cleanly.
        let corrupt_before = corrupt_entries();
        let reopened_mid = reopened_replicas();
        fleet.mgr("laptop-b")?.inject_raw_proposal("sess-fence", b"{not-valid-convo-entry".to_vec())?;
        fleet.mgr("laptop-b")?.drive_due();
        anyhow::ensure!(!fleet.mgr("laptop-b")?.flush(TEST_WAIT), "a permanently corrupt committed command cannot report a clean flush");
        anyhow::ensure!(corrupt_entries() > corrupt_before, "undecodable entry must increment corrupt_entries metric");
        // A reopen fenced again before committing cleanly is held: no tight-loop reopens on a permanently corrupt entry.
        let reopened_after = reopened_replicas();
        anyhow::ensure!(reopened_after <= reopened_mid + 2, "a held reopen must stop the permanent corrupt-entry loop: before={reopened_mid} after={reopened_after}");
        Ok(())
    }

    /// Verifies that a mid-chunk leader crash leaves NO permanently stranded `in_flight_parts`:
    /// when a new leader wins election (or the restarted node campaigns in `term + 1`), its term
    /// advance deterministically purges the incomplete chunk and unblocks compaction and writes.
    #[test]
    fn stranded_partial_chunk_purged_on_new_term_and_unblocks_compaction() -> anyhow::Result<()> {
        let wal_dir = tempfile::tempdir()?;
        let (_sdir1, store_1) = crate::convo::store::scratch()?;
        {
            let mgr = Manager::open("laptop-b", wal_dir.path(), Some(store_1.clone()))?;
            let open_row = Row::Session {
                session: "sess-crash-chunk".into(),
                info: Some(r#"{"id":"sess-crash-chunk","target":"agent:laptop-b"}"#.into()),
                meta: None,
                state: None,
                changes: None,
                epoch: Some(1),
                open: true,
                at: 1000,
            };
            let ev1 = user_event("sess-crash-chunk", 1, "before-partial");
            store_1.put(vec![open_row.clone(), ev1.clone()]);
            mgr.open_leader("sess-crash-chunk", 1, vec![open_row, ev1])?;
            // Commit ONLY part 0 of 2 (simulating a leader crash before part 1 is proposed!).
            let orphan_part = serde_json::to_vec(&WirePart {
                chunk_id: 4242,
                part: 0,
                total: 2,
                chunk_b64: base64::engine::general_purpose::STANDARD.encode(b"half-json").into(),
            })?;
            mgr.inject_raw_proposal("sess-crash-chunk", orphan_part)?;
            anyhow::ensure!(mgr.flush(TEST_WAIT));
            let (_, _, in_flight) = mgr.snapshot_state("sess-crash-chunk")?;
            anyhow::ensure!(in_flight == 1, "partial chunk must be recorded before crash, got {in_flight}");
            mgr.shutdown();
        }
        // Restart node: campaigning in `term + 1` commits a leader no-op in the new term, which
        // deterministically purges the stranded part 0/2 and unblocks compaction!
        let (_sdir2, store_2) = crate::convo::store::scratch()?;
        let mgr_after = Manager::open("laptop-b", wal_dir.path(), Some(store_2.clone()))?;
        anyhow::ensure!(mgr_after.flush(TEST_WAIT));
        store_2.sync()?;
        let (_, _, in_flight_after) = mgr_after.snapshot_state("sess-crash-chunk")?;
        anyhow::ensure!(in_flight_after == 0, "new term must purge stranded mid-chunk entry, got {in_flight_after}");
        mgr_after.force_compact("sess-crash-chunk", 0)?;
        anyhow::ensure!(mgr_after.flush(TEST_WAIT));
        Ok(())
    }

    /// Retain the committed close until an offline member catches up.
    #[test]
    fn closed_session_retains_its_wal_for_an_offline_member() -> anyhow::Result<()> {
        let fleet = Fleet::new(&["laptop-a", "laptop-b"])?;
        // Register a permanently offline third peer and mark it unreachable.
        fleet.mgr("laptop-b")?.register_peers(["laptop-a", "laptop-dead"]);
        fleet.mgr("laptop-b")?.peer_unreachable("laptop-dead");

        let open_row = Row::Session {
            session: "sess-retire-dead".into(),
            info: Some(r#"{"id":"sess-retire-dead","target":"agent:laptop-b"}"#.into()),
            meta: None,
            state: None,
            changes: None,
            epoch: Some(1),
            open: true,
            at: 1000,
        };
        fleet.store("laptop-b")?.put(vec![open_row.clone()]);
        fleet.open("laptop-b", "sess-retire-dead", 1, vec![open_row])?;
        fleet.settle()?;

        let closed = Row::Closed { session: "sess-retire-dead".into(), at: 2000 };
        fleet.store("laptop-b")?.put(vec![closed.clone()]);
        fleet.mgr("laptop-b")?.propose("sess-retire-dead", 1, vec![closed])?;
        fleet.settle()?;

        anyhow::ensure!(fleet.mgr("laptop-b")?.status("sess-retire-dead").is_some(), "offline member must still be able to recover the close from its leader");
        Ok(())
    }

    /// Verifies that a failing snapshot build is held, until the group applies past it or an event
    /// clears it, instead of spawning snapshot worker threads on every drive pass.
    #[test]
    fn failed_snapshot_build_backs_off_without_spinning() -> anyhow::Result<()> {
        let wal_dir = tempfile::tempdir()?;
        let (_sdir, store) = crate::convo::store::scratch()?;
        let mgr = Manager::open("laptop-b", wal_dir.path(), Some(store.clone()))?;
        // Inject a projection newer than the pinned Raft point to exercise the read failure.
        mgr.open_leader("sess-snap-fail", 1, vec![user_event("sess-snap-fail", 1, "missing-from-store")])?;
        anyhow::ensure!(mgr.flush(TEST_WAIT));

        let st = mgr.status("sess-snap-fail").context("missing session")?;
        store.apply_raft_rows("sess-snap-fail", st.applied + 1, st.term, vec![user_event("sess-snap-fail", 2, "ahead")])?;
        let err = mgr.force_compact("sess-snap-fail", 0);
        anyhow::ensure!(err.is_err(), "inconsistent store must fail snapshot build: {err:?}");
        let (failures, backed_off, _) = mgr.snapshot_state("sess-snap-fail")?;
        anyhow::ensure!(failures == 1 && backed_off, "failed snapshot must increment failure count ({failures}) and be held ({backed_off})");
        Ok(())
    }

    /// Verifies group lifecycle retirement: once a session commits `Row::Closed` and every follower
    /// catches up durably, the leader retires the group (freeing its WAL slot, removing its state
    /// checkpoint and `sessions.json` entry, incrementing `retired_groups()`) while keeping the
    /// closed session intact in every node's `Store`.
    #[test]
    fn closed_session_retires_wal_group_once_followers_catch_up() -> anyhow::Result<()> {
        let fleet = Fleet::new(&["laptop-a", "laptop-b"])?;
        let retired_before = retired_groups();
        let open_row = Row::Session {
            session: "sess-retire".into(),
            info: Some(r#"{"id":"sess-retire","target":"agent:laptop-b"}"#.into()),
            meta: None,
            state: None,
            changes: None,
            epoch: Some(1),
            open: true,
            at: 1000,
        };
        fleet.store("laptop-b")?.put(vec![open_row.clone()]);
        fleet.open("laptop-b", "sess-retire", 1, vec![open_row])?;
        fleet.mgr("laptop-b")?.propose("sess-retire", 1, vec![user_event("sess-retire", 1, "final msg")])?;
        fleet.settle()?;

        // Close the session and replicate `Row::Closed` across the fleet.
        let closed = Row::Closed { session: "sess-retire".into(), at: 2000 };
        fleet.store("laptop-b")?.put(vec![closed.clone()]);
        fleet.mgr("laptop-b")?.propose("sess-retire", 1, vec![closed])?;
        fleet.settle()?;

        // Leader must retire the closed WAL group once laptop-a confirms holding the close entry!
        anyhow::ensure!(retired_groups() > retired_before, "leader must retire closed WAL group once followers catch up");
        anyhow::ensure!(fleet.mgr("laptop-b")?.status("sess-retire").is_none(), "retired group must be removed from leader active map");
        let index_raw = std::fs::read(fleet.wal_dir("laptop-b")?.join("sessions.json"))?;
        let slots: BTreeMap<String, SessionSlot> = serde_json::from_slice(&index_raw)?;
        anyhow::ensure!(!slots.contains_key("sess-retire"), "retired group must be removed from sessions.json");

        anyhow::ensure!(fleet.open("laptop-b", "sess-retire", 1, Vec::new()).is_err(), "a stale founder cannot reuse the archived session");
        let mut fleet = fleet;
        fleet.add_node("after-close")?;
        fleet.settle()?;
        let late = fleet.store("after-close")?.kept("sess-retire", 10)?.context("late member lacks archived conversation")?;
        anyhow::ensure!(late.closed_at == Some(2000) && late.events.len() == 1, "post-close enrollment must receive the terminal image");
        // The closed session and its events remain durably preserved in follower's Store!
        fleet.store("laptop-a")?.sync()?;
        let kept = fleet.store("laptop-a")?.kept("sess-retire", 10)?.ok_or_else(|| anyhow::anyhow!("follower missing closed session"))?;
        anyhow::ensure!(kept.closed_at == Some(2000) && kept.events.len() == 1, "follower Store must preserve closed session + events: {kept:?}");
        Ok(())
    }

    /// Verifies W27 custody & sender authentication + non-voter proposal refusal:
    /// 1. A Raft frame whose `msg.from` disagrees with authenticated `from_node` is refused.
    /// 2. A peer cannot hijack a session already owned locally at the same epoch.
    /// 3. Cross-session rows stuffed into another session's Raft entry are dropped before `Store::put`.
    /// 4. Proposing on a follower/learner replica returns an explicit `Err` and increments `dropped_proposals()`.
    #[test]
    fn spoofed_sender_and_cross_session_rows_are_refused() -> anyhow::Result<()> {
        let (_sdir, store) = crate::convo::store::scratch()?;
        let wal_dir = tempfile::tempdir()?;
        let mgr = Manager::open("laptop-a", wal_dir.path(), Some(store.clone()))?;

        let spoofed = Message {
            msg_type: MessageType::MsgHeartbeat,
            from: node_id("laptop-b"),
            to: node_id("laptop-a"),
            term: 1,
            ..Message::default()
        };
        mgr.step("laptop-evil", "sess-spoof", &spoofed.encode_to_vec())?;
        anyhow::ensure!(mgr.flush(TEST_WAIT));
        anyhow::ensure!(mgr.status("sess-spoof").is_none(), "spoofed msg.from must never create a Raft group");

        store.put(vec![Row::Session {
            session: "sess-mine".into(),
            info: Some(r#"{"id":"sess-mine","target":"agent:laptop-a"}"#.into()),
            meta: None,
            state: Some(r#"{"epoch":2}"#.into()),
            changes: None,
            epoch: Some(2),
            open: true,
            at: 1000,
        }]);
        store.sync()?;
        let hijack = Message {
            msg_type: MessageType::MsgHeartbeat,
            from: node_id("laptop-evil"),
            to: node_id("laptop-a"),
            term: 1,
            ..Message::default()
        };
        mgr.step("laptop-evil", "sess-mine", &hijack.encode_to_vec())?;
        anyhow::ensure!(mgr.flush(TEST_WAIT));
        anyhow::ensure!(mgr.status("sess-mine").is_none(), "peer must not initiate group for session already owned locally");

        let fleet = Fleet::new(&["laptop-a", "laptop-b"])?;
        fleet.open("laptop-b", "sess-a", 1, Vec::new())?;
        anyhow::ensure!(fleet.mgr("laptop-b")?.propose("sess-a", 1, vec![user_event("sess-victim", 1, "injected"), user_event("sess-a", 1, "mixed")]).is_err());
        fleet.mgr("laptop-b")?.propose("sess-a", 1, vec![user_event("sess-a", 2, "legit")])?;
        fleet.settle()?;
        fleet.store("laptop-a")?.sync()?;
        anyhow::ensure!(fleet.store("laptop-a")?.kept("sess-victim", 10)?.is_none(), "cross-session row must never land in follower Store");
        anyhow::ensure!(fleet.store("laptop-a")?.kept("sess-a", 10)?.is_some_and(|k| k.events.len() == 1), "legit row must replicate");

        // Non-voter `propose_sync` on follower laptop-a must return Err and increment dropped_proposals!
        let drops_before = dropped_proposals();
        let refused = fleet.mgr("laptop-a")?.propose_sync("sess-a", 1, vec![user_event("sess-a", 2, "from-learner")]);
        anyhow::ensure!(refused.is_err(), "proposing on non-voter learner replica must return Err");
        anyhow::ensure!(dropped_proposals() > drops_before, "non-voter proposal refusal must increment dropped_proposals");
        Ok(())
    }

    /// Verifies that an entry-less heartbeat from a malicious peer cannot bypass custody checks
    /// when a subsequent `Row::Session` attempts to claim another runner's `info.target`.
    #[test]
    fn heartbeat_cannot_bypass_session_target_custody_check() -> anyhow::Result<()> {
        let (_sdir, store) = crate::convo::store::scratch()?;
        let wal_dir = tempfile::tempdir()?;
        let mgr = Manager::open("laptop-a", wal_dir.path(), Some(store.clone()))?;

        // Step 1: entry-less heartbeat creates follower group for laptop-evil.
        let hb = Message {
            msg_type: MessageType::MsgHeartbeat,
            from: node_id("laptop-evil"),
            to: node_id("laptop-a"),
            term: 1,
            ..Message::default()
        };
        mgr.step("laptop-evil", "sess-bypass", &hb.encode_to_vec())?;
        anyhow::ensure!(mgr.flush(TEST_WAIT));

        // Step 2: laptop-evil sends a committed entry claiming `target: "agent:laptop-victim"`.
        let evil_entry = ConvoEntry {
            process_loss:None, owner_run:None,
            reaffirm: false, seal_voters: Vec::new(),
            genesis_charter: None, genesis_members: Vec::new(), seal: None, opening: None,
            policy: None, claim_from: None, native_move: None,
            session: "sess-bypass".into(),
            leader: "laptop-evil".into(),
            epoch: 1,
            rows: vec![Row::Session {
                session: "sess-bypass".into(),
                info: Some(r#"{"id":"sess-bypass","target":"agent:laptop-victim"}"#.into()),
                meta: None,
                state: None,
                changes: None,
                epoch: Some(1),
                open: true,
                at: 1000,
            }],
        };
        let app = Message {
            msg_type: MessageType::MsgAppend,
            from: node_id("laptop-evil"),
            to: node_id("laptop-a"),
            term: 1,
            index: 0,
            log_term: 0,
            commit: 1,
            entries: vec![hyper_raft::proto::Entry {
                term: 1,
                index: 1,
                entry_type: EntryType::EntryNormal,
                data: serde_json::to_vec(&evil_entry)?,
                ..Default::default()
            }],
            ..Message::default()
        };
        mgr.step("laptop-evil", "sess-bypass", &app.encode_to_vec())?;
        anyhow::ensure!(mgr.flush(TEST_WAIT));
        store.sync()?;
        anyhow::ensure!(store.kept("sess-bypass", 10)?.is_none(), "spoofed target row after heartbeat must be dropped");
        Ok(())
    }

    /// Verifies that snapshotting 501+ events with duplicate `seq` values straddling the 500-row
    /// `SNAPSHOT_PAGE` boundary preserves EVERY SINGLE event (zero omissions!) via `(seq, rowid)`.
    #[test]
    fn snapshot_cursor_handles_501_events_with_duplicate_seq_at_page_boundary() -> anyhow::Result<()> {
        let mut fleet = Fleet::new(&["laptop-b"])?;
        let open_row = Row::Session {
            session: "sess-tie".into(),
            info: Some(r#"{"id":"sess-tie","target":"agent:laptop-b"}"#.into()),
            meta: None,
            state: None,
            changes: None,
            epoch: Some(1),
            open: true,
            at: 1000,
        };
        fleet.store("laptop-b")?.put(vec![open_row.clone()]);
        fleet.open("laptop-b", "sess-tie", 1, vec![open_row])?;

        // Write 501 distinct events where two share `seq = 500` right at the 500-row boundary!
        let mut batch = Vec::with_capacity(501);
        for i in 1..=499u64 {
            batch.push(user_event("sess-tie", i, &format!("ev-{i}")));
        }
        for (suffix, text) in [("a", "tie-500-a"), ("b", "tie-500-b")] {
            batch.push(Row::Event(Box::new(EventRow {
                session: "sess-tie".into(),
                id: format!("ev-500-{suffix}"),
                seq: 500,
                at: 2000,
                parent: None,
                by: Some("user:ada".into()),
                kind: "user".into(),
                level: None,
                tool: None,
                status: None,
                text: text.into(),
                body: serde_json::json!({"id": format!("ev-500-{suffix}"), "seq": 500, "at": 2000, "body": {"kind": "user", "text": text}}).to_string(),
            })));
        }
        fleet.store("laptop-b")?.put(batch.clone());
        fleet.mgr("laptop-b")?.propose("sess-tie", 1, batch)?;
        fleet.settle()?;
        fleet.mgr("laptop-b")?.force_compact("sess-tie", 0)?;
        fleet.settle()?;

        fleet.add_node("laptop-late")?;
        fleet.settle()?;
        fleet.store("laptop-late")?.sync()?;
        let kept = fleet.store("laptop-late")?.kept("sess-tie", 600)?.ok_or_else(|| anyhow::anyhow!("missing sess-tie on late peer"))?;
        anyhow::ensure!(kept.events.len() == 501, "snapshot must preserve all 501 events across duplicate seq boundary, got {}", kept.events.len());
        anyhow::ensure!(kept.events.iter().any(|e| e.text == "tie-500-a") && kept.events.iter().any(|e| e.text == "tie-500-b"), "both duplicate-seq events at boundary must survive");
        Ok(())
    }

    /// Verifies that installing a snapshot atomically wipes previously deleted inbox rows and
    /// deleted record sources on the follower instead of leaving stale rows behind.
    #[test]
    fn snapshot_install_clears_deleted_inbox_and_record_sources() -> anyhow::Result<()> {
        let mut fleet = Fleet::new(&["laptop-b"])?;
        let open_row = Row::Session {
            session: "sess-wipe".into(),
            info: Some(r#"{"id":"sess-wipe","target":"agent:laptop-b"}"#.into()),
            meta: None,
            state: None,
            changes: None,
            epoch: Some(1),
            open: true,
            at: 1000,
        };
        let live_ev = user_event("sess-wipe", 1, "live");
        let live_inbox = Row::Inbox(Box::new(InboxRow {
            session: "sess-wipe".into(),
            family: "inbox".into(),
            key: b"sess-wipe/keep-me".to_vec(),
            value: Some(br#"{"epoch":1}"#.to_vec()),
        }));
        let init = vec![open_row, live_ev, live_inbox];
        fleet.store("laptop-b")?.put(init.clone());
        fleet.open("laptop-b", "sess-wipe", 1, init)?;
        fleet.settle()?;
        fleet.mgr("laptop-b")?.force_compact("sess-wipe", 0)?;
        fleet.settle()?;

        // Pre-populate a late follower's Store with stale/deleted inbox & record rows for `sess-wipe`.
        fleet.add_node("laptop-stale")?;
        fleet.store("laptop-stale")?.put(vec![
            Row::Inbox(Box::new(InboxRow {
                session: "sess-wipe".into(),
                family: "inbox".into(),
                key: b"sess-wipe/deleted-mail".to_vec(),
                value: Some(br#"{"epoch":0}"#.to_vec()),
            })),
            Row::Record(Box::new(RecordRow {
                session: "sess-wipe".into(),
                source: "deleted-transcript".into(),
                n: 0,
                end: 10,
                line: "stale".into(),
                kept: None,
            })),
        ]);
        fleet.store("laptop-stale")?.sync()?;
        fleet.settle()?;
        fleet.store("laptop-stale")?.sync()?;

        let inbox = fleet.store("laptop-stale")?.inbox_rows("inbox", b"sess-wipe/".to_vec())?;
        anyhow::ensure!(!inbox.iter().any(|(k, _, _)| k == b"sess-wipe/deleted-mail"), "snapshot install must wipe deleted inbox rows: {inbox:?}");
        anyhow::ensure!(inbox.iter().any(|(k, _, _)| k == b"sess-wipe/keep-me"), "snapshot install must keep live inbox rows: {inbox:?}");
        let marks = fleet.store("laptop-stale")?.marks("sess-wipe")?;
        anyhow::ensure!(marks.is_empty(), "snapshot install must wipe deleted record sources: {marks:?}");
        Ok(())
    }

    /// Verifies that a fence in the middle of a chunked oversized row resets chunk state on reopen,
    /// re-proposes the ENTIRE oversized row cleanly under a fresh `chunk_id` in `term + 1`, and
    /// replicates it intact to followers instead of hitting permanent `Fatal`!
    #[test]
    fn fence_mid_chunk_reproposes_whole_row_in_new_term() -> anyhow::Result<()> {
        let fleet = Fleet::new(&["laptop-a", "laptop-b"])?;
        let open_row = Row::Session {
            session: "sess-fence-chunk".into(),
            info: Some(r#"{"id":"sess-fence-chunk","target":"agent:laptop-b"}"#.into()),
            meta: None,
            state: None,
            changes: None,
            epoch: Some(1),
            open: true,
            at: 1000,
        };
        fleet.store("laptop-b")?.put(vec![open_row.clone()]);
        fleet.open("laptop-b", "sess-fence-chunk", 1, vec![open_row])?;
        fleet.settle()?;

        // Inject part 0 of 2 AND fail next persist so a reopen occurs in a new term while an
        // oversized row is proposed!
        let orphan_part = serde_json::to_vec(&WirePart {
            chunk_id: 9999,
            part: 0,
            total: 2,
            chunk_b64: base64::engine::general_purpose::STANDARD.encode(b"half").into(),
        })?;
        fleet.mgr("laptop-b")?.inject_raw_proposal("sess-fence-chunk", orphan_part)?;
        fleet.settle()?;

        fleet.mgr("laptop-b")?.fail_next_persist("sess-fence-chunk")?;
        let _ = fleet.mgr("laptop-b")?.force_compact("sess-fence-chunk", 0);

        let huge_payload = "M".repeat(1200 * 1024);
        let huge_ev = user_event("sess-fence-chunk", 1, &huge_payload);
        fleet.store("laptop-b")?.put(vec![huge_ev.clone()]);
        fleet.settle()?;
        let epoch = fleet.mgr("laptop-b")?.status("sess-fence-chunk").context("missing recovered custody")?.epoch;
        fleet.mgr("laptop-b")?.propose("sess-fence-chunk", epoch, vec![huge_ev])?;
        fleet.settle()?;

        fleet.store("laptop-a")?.sync()?;
        let kept = fleet.store("laptop-a")?.kept("sess-fence-chunk", 10)?.ok_or_else(|| anyhow::anyhow!("follower missing session after mid-chunk fence"))?;
        anyhow::ensure!(
            kept.events.len() == 1 && kept.events.first().is_some_and(|e| e.text.len() == huge_payload.len()),
            "oversized row must survive mid-chunk fence and replicate intact in new term"
        );
        Ok(())
    }

    /// Verifies that a transient reopen failure on one session in `Owner::open` NEVER erases that
    /// session from `sessions.json` and orphans its intact WAL!
    #[test]
    fn failed_reopen_preserves_session_in_sessions_index() -> anyhow::Result<()> {
        let wal_dir = tempfile::tempdir()?;
        let (_sdir1, store_1) = crate::convo::store::scratch()?;
        let gid_bad = group_id("sess-corrupt-state");
        {
            let mgr = Manager::open("laptop-b", wal_dir.path(), Some(store_1))?;
            mgr.open_leader("sess-ok", 1, vec![user_event("sess-ok", 1, "ok")])?;
            mgr.open_leader("sess-corrupt-state", 1, vec![user_event("sess-corrupt-state", 1, "bad")])?;
            anyhow::ensure!(mgr.flush(TEST_WAIT));
            mgr.shutdown();
        }
        // Corrupt ONLY `sess-corrupt-state`'s state file temporarily so its reopen fails once.
        let state_path = wal_dir.path().join("state").join(format!("{gid_bad:032x}.json"));
        let backup = std::fs::read(&state_path)?;
        std::fs::write(&state_path, b"{not-json")?;
        {
            let (_sdir2, store_2) = crate::convo::store::scratch()?;
            let mgr = Manager::open("laptop-b", wal_dir.path(), Some(store_2))?;
            // Creating a third session triggers `save_sessions_index()`.
            mgr.open_leader("sess-third", 1, vec![user_event("sess-third", 1, "third")])?;
            anyhow::ensure!(!mgr.flush(TEST_WAIT), "the damaged session must be reported while the third session remains healthy");
            mgr.shutdown();
        }
        // Verify `sess-corrupt-state` was NOT erased from `sessions.json`, and recovers once repaired!
        let slots: BTreeMap<String, SessionSlot> = serde_json::from_slice(&std::fs::read(wal_dir.path().join("sessions.json"))?)?;
        anyhow::ensure!(slots.contains_key("sess-corrupt-state"), "failed reopen must preserve slot in sessions.json: {slots:?}");
        std::fs::write(&state_path, backup)?;
        let (_sdir3, store_3) = crate::convo::store::scratch()?;
        let mgr_recovered = Manager::open("laptop-b", wal_dir.path(), Some(store_3))?;
        anyhow::ensure!(mgr_recovered.flush(TEST_WAIT));
        anyhow::ensure!(mgr_recovered.status("sess-corrupt-state").is_some(), "preserved WAL group must recover cleanly once state file is intact");
        Ok(())
    }

    /// Verifies that two distinct server installations receive unique persistent Raft identities
    /// via `server_node_name` so they NEVER collide on member ID 1.
    #[test]
    fn two_servers_get_distinct_raft_identities_without_colliding() -> anyhow::Result<()> {
        let dir_a = tempfile::tempdir()?;
        let dir_b = tempfile::tempdir()?;
        let name_a = server_node_name(dir_a.path())?;
        let name_b = server_node_name(dir_b.path())?;
        anyhow::ensure!(name_a != name_b, "two servers must have distinct node names: {name_a} vs {name_b}");
        anyhow::ensure!(node_id(&name_a) != node_id(&name_b), "two servers must have distinct Raft member IDs");
        anyhow::ensure!(node_id(&name_a) != SERVER_ID && node_id(&name_b) != SERVER_ID, "unique server ids must not collide on legacy id 1");
        anyhow::ensure!(server_node_name(dir_a.path())? == name_a, "server node name must persist across restarts");
        Ok(())
    }

    /// Verifies that a closed session does NOT retire prematurely while a connected (non-unreachable)
    /// follower is still behind — only retiring once that follower actually receives `Row::Closed`!
    #[test]
    fn closed_session_waits_for_connected_follower_before_retiring() -> anyhow::Result<()> {
        let fleet = Fleet::new(&["laptop-a", "laptop-b"])?;
        let open_row = Row::Session {
            session: "sess-no-premature-retire".into(),
            info: Some(r#"{"id":"sess-no-premature-retire","target":"agent:laptop-b"}"#.into()),
            meta: None,
            state: None,
            changes: None,
            epoch: Some(1),
            open: true,
            at: 1000,
        };
        fleet.store("laptop-b")?.put(vec![open_row.clone()]);
        fleet.open("laptop-b", "sess-no-premature-retire", 1, vec![open_row])?;
        fleet.settle()?;

        // Propose `Row::Closed` while laptop-a is still connected (NOT marked unreachable) and
        // pause message delivery to laptop-a: leader MUST NOT retire while laptop-a is behind!
        let closed = Row::Closed { session: "sess-no-premature-retire".into(), at: 2000 };
        fleet.store("laptop-b")?.put(vec![closed.clone()]);
        fleet.mgr("laptop-b")?.propose("sess-no-premature-retire", 1, vec![closed])?;
        fleet.detector_partition(&[],false)?;
        fleet.settle_except(&["laptop-a"])?;
        anyhow::ensure!(
            fleet.mgr("laptop-b")?.status("sess-no-premature-retire").is_some(),
            "leader must NOT retire closed group while connected peer laptop-a has not caught up"
        );

        // Once laptop-a receives the close entry, leader retires cleanly!
        fleet.settle()?;
        let leader=fleet.mgr("laptop-b")?.status("sess-no-premature-retire");
        anyhow::ensure!(leader.is_none(), "leader must retire once laptop-a catches up: {leader:?}, follower {:?}",fleet.mgr("laptop-a")?.status("sess-no-premature-retire"));
        fleet.store("laptop-a")?.sync()?;
        anyhow::ensure!(fleet.store("laptop-a")?.kept("sess-no-premature-retire", 10)?.is_some_and(|k| k.closed_at == Some(2000)));
        Ok(())
    }

    /// Automatic failover acceptance test (3-node fleet `{node-a, node-b, node-c}`):
    /// 1. Start session on B -> B elects itself initial leader and automatically promotes A and C
    ///    to voters (3-voter odd quorum `[B, A, C]`).
    /// 2. B accepts message `m-failover-1` into its real `Inbox` and replicates history + inbox
    ///    custody through Raft to A and C.
    /// 3. Kill B WITHOUT any handoff!
    /// 4. Surviving voting majority `{A, C}` automatically elects a successor (A or C), emits
    ///    `FailoverEvent::Claimed { new_epoch: 2, .. }`, claims epoch 2 in Raft, resumes its
    ///    `Inbox` at epoch 2, and accounts for `m-failover-1` **EXACTLY ONCE** (`repeat: true` on retry!).
    /// 5. When B returns, B receives the higher term/epoch, emits `FailoverEvent::LostLeadership`,
    ///    and is UNABLE to act under its old ownership epoch (both Raft `propose(epoch=1)` and B's
    ///    fenced `Inbox` refuse!).
    #[test]
    fn automatic_failover_elects_survivor_claims_new_epoch_and_fences_returning_leader() -> anyhow::Result<()> {
        use crate::harness::inbox::{Inbox, Limits};
        use crate::convo::inbox::InboxRecords;

        let fleet = Fleet::new_with_policy(&["node-a", "node-b", "node-c"], Policy { auto_failover: true, ..Policy::default() })?;
        let inbox_a = Arc::new(Inbox::new(Arc::new(InboxRecords::new(crate::convo::inbox::Source::Store(fleet.store("node-a")?.clone()), crate::harness::inbox::FAMILIES, None)), Limits::default()));
        let inbox_b = Arc::new(Inbox::new(Arc::new(InboxRecords::new(crate::convo::inbox::Source::Store(fleet.store("node-b")?.clone()), crate::harness::inbox::FAMILIES, None)), Limits::default()));
        let inbox_c = Arc::new(Inbox::new(Arc::new(InboxRecords::new(crate::convo::inbox::Source::Store(fleet.store("node-c")?.clone()), crate::harness::inbox::FAMILIES, None)), Limits::default()));

        // 1. Start session on node-b (epoch 1) and settle so all 3 nodes form a 3-voter quorum!
        let sess = "sess-failover-1";
        let open_row = Row::Session {
            session: sess.into(),
            info: Some(r#"{"id":"sess-failover-1","kind":"claude","target":"agent:node-b","epoch":1}"#.into()),
            meta: None,
            state: Some(r#"{"epoch":1,"status":"waiting"}"#.into()),
            changes: None,
            epoch: Some(1),
            open: true,
            at: 1000,
        };
        let ev1 = user_event(sess, 1, "initial prompt on B");
        fleet.store("node-b")?.put(vec![open_row.clone(), ev1.clone()]);
        fleet.open("node-b", sess, 1, vec![open_row, ev1])?;
        fleet.settle()?;

        let st_b = fleet.mgr("node-b")?.status(sess).ok_or_else(|| anyhow::anyhow!("missing status on B"))?;
        anyhow::ensure!(st_b.is_leader && st_b.voters.len() == 3, "3-node fleet must auto-promote to 3-voter quorum, got {st_b:?}");

        // 2. B accepts message `m-failover-1` into its real Inbox and replicates inbox custody via Raft!
        let digest = crate::harness::inbox::digest("do critical work", &[]);
        let ans_b = {
            let (ib, dig) = (Arc::clone(&inbox_b), digest.clone());
            crate::rt::block_on(async move {
                ib.open(sess, 1).await.map_err(|e| anyhow::anyhow!("{e}"))?;
                ib.accept(sess, "m-failover-1", &dig, Some(1), 1500).await.map_err(|e| anyhow::anyhow!("{e}"))
            })?
        };
        anyhow::ensure!(!ans_b.repeat && ans_b.epoch == 1, "B must accept m-failover-1 once at epoch 1: {ans_b:?}");
        fleet.store("node-b")?.sync()?;
        let inbox_rows = session_inbox_rows(fleet.store("node-b")?, sess)?;
        anyhow::ensure!(!inbox_rows.is_empty(), "B must have inbox rows to replicate");
        fleet.mgr("node-b")?.propose(sess, 1, inbox_rows)?;
        fleet.settle()?;

        // 3. Kill B WITHOUT any handoff! Deliver the detector verdict and run measured elections on surviving members A and C.
        fleet.elect_survivor(&["node-b"],sess)?;

        let fail_a = fleet.mgr("node-a")?.failover_events();
        let fail_c = fleet.mgr("node-c")?.failover_events();
        let mut winner: Option<(String, u64)> = None;
        for (name, rx) in [("node-a", fail_a), ("node-c", fail_c)] {
            while let Ok(ev) = rx.try_recv() {
                if let FailoverEvent::Claimed { session, new_epoch, previous_leader, .. } = ev
                    && session == sess
                {
                    anyhow::ensure!(previous_leader == "node-b", "previous leader must be node-b, got {previous_leader}");
                    anyhow::ensure!(new_epoch >= 2, "failover claim must advance epoch >= 2, got {new_epoch}");
                    winner = Some((name.to_owned(), new_epoch));
                }
            }
        }
        let (winner_name, new_epoch) = winner.ok_or_else(|| anyhow::anyhow!("surviving majority {{A, C}} must elect a failover leader and emit Claimed"))?;
        let winner_st = fleet.mgr(&winner_name)?.status(sess).ok_or_else(|| anyhow::anyhow!("missing winner status"))?;
        anyhow::ensure!(winner_st.is_leader && winner_st.epoch == new_epoch, "winner must lead at epoch {new_epoch}: {winner_st:?}");

        // 4. Winner resumes its Inbox from replicated Store at `new_epoch`: retrying `m-failover-1`
        //    must return `repeat: true` (EXACTLY ONCE!) and a brand-new message succeeds at `new_epoch`!
        let winner_store = fleet.store(&winner_name)?;
        winner_store.sync()?;
        let (brought, _waiting) = crate::harness::inbox::carried_from(winner_store, sess)?;
        let winner_inbox = if winner_name == "node-a" { Arc::clone(&inbox_a) } else { Arc::clone(&inbox_c) };
        let (retry_ans, fresh_ans) = {
            let dig = digest.clone();
            crate::rt::block_on(async move {
                winner_inbox.arrive(sess, new_epoch, &brought, false, 2000).await.map_err(|e| anyhow::anyhow!("{e}"))?;
                let retry = winner_inbox.accept(sess, "m-failover-1", &dig, Some(new_epoch), 2100).await.map_err(|e| anyhow::anyhow!("{e}"))?;
                let d2 = crate::harness::inbox::digest("follow-up after failover", &[]);
                let fresh = winner_inbox.accept(sess, "m-after-failover", &d2, Some(new_epoch), 2200).await.map_err(|e| anyhow::anyhow!("{e}"))?;
                Ok::<_, anyhow::Error>((retry, fresh))
            })?
        };
        anyhow::ensure!(retry_ans.repeat, "retrying m-failover-1 after failover MUST deduplicate (repeat=true): {retry_ans:?}");
        anyhow::ensure!(!fresh_ans.repeat && fresh_ans.epoch == new_epoch, "new message after failover must succeed at new_epoch: {fresh_ans:?}");

        // 5. Bring B back online: B must step down, emit `LostLeadership`, fence its old inbox, and
        //    be UNABLE to propose or accept under its old ownership epoch (epoch 1)!
        let deadline = Instant::now() + TEST_WAIT;
        while fleet.mgr("node-b")?.status(sess).is_none_or(|s| s.owner.as_deref() != Some(winner_name.as_str()) || s.epoch != new_epoch) && Instant::now() < deadline {
            fleet.settle()?;
        }
        let fail_b = fleet.mgr("node-b")?.failover_events();
        let observed:Vec<_>=std::iter::from_fn(||fail_b.try_recv().ok()).collect();
        let b_lost=observed.iter().any(|event|matches!(event,FailoverEvent::LostLeadership{session,new_epoch:e,new_leader,..}
            if session==sess && *e==new_epoch && new_leader==&winner_name));
        anyhow::ensure!(b_lost, "returning former leader B must receive LostLeadership for new leader {winner_name} at epoch {new_epoch}: {observed:?}, status {:?}",fleet.mgr("node-b")?.status(sess));
        let stale_propose = fleet.mgr("node-b")?.propose_sync(sess, 1, vec![user_event(sess, 99, "stale-write-from-B")]);
        anyhow::ensure!(stale_propose.is_err(), "returning B must be unable to propose under old epoch 1: {stale_propose:?}");

        // Fencing B's local inbox on LostLeadership also rejects any late send under old epoch 1.
        let b_inbox_refused = crate::rt::block_on(async move {
            let _ = inbox_b.fence(sess, 1).await;
            let res = inbox_b.accept(sess, "m-stale-to-b", &crate::harness::inbox::digest("late", &[]), Some(1), 2500).await;
            Ok::<_, anyhow::Error>(res.is_err())
        })?;
        anyhow::ensure!(b_inbox_refused, "fenced old leader B's inbox must refuse old-epoch send");
        Ok(())
    }

    /// Verifies that an isolated leader partitioned from its 3-voter majority advances its
    /// detector loses its trusted quorum and it steps down and emits `LostLeadership`
    /// even before reconnecting to the new leader (so its harness cannot keep accepting work!).
    #[test]
    fn isolated_leader_steps_down_via_check_quorum_and_emits_lost_leadership() -> anyhow::Result<()> {
        let fleet = Fleet::new_with_policy(&["node-a", "node-b", "node-c"], Policy { auto_failover: true, ..Policy::default() })?;
        let sess = "sess-quorum-fence";
        let open_row = Row::Session {
            session: sess.into(),
            info: Some(r#"{"id":"sess-quorum-fence","kind":"claude","target":"agent:node-b","epoch":1}"#.into()),
            meta: None,
            state: Some(r#"{"epoch":1}"#.into()),
            changes: None,
            epoch: Some(1),
            open: true,
            at: 1000,
        };
        fleet.store("node-b")?.put(vec![open_row.clone()]);
        fleet.open("node-b", sess, 1, vec![open_row])?;
        fleet.settle()?;
        anyhow::ensure!(fleet.mgr("node-b")?.status(sess).is_some_and(|s| s.is_leader && s.voters.len() == 3));

        // Partition B from A and C: tick ONLY B (no responses from A or C!).
        // `check_quorum` must step B down within `(TEST_WAIT / 2)` and emit `LostLeadership`!
        let rx_b = fleet.mgr("node-b")?.failover_events();
        // The first quorum check still counts answers from before the partition.
        fleet.detector_partition(&["node-b"],true)?;
        let quorum_ticks = raft_settings(node_id("node-b"), 1)?.core.election_tick;
        for _ in 0..quorum_ticks.saturating_mul(2) {
            fleet.mgr("node-b")?.drive_due();
            anyhow::ensure!(fleet.mgr("node-b")?.flush(TEST_WAIT));
            let out = fleet.mgr("node-b")?.outgoing();
            while out.try_recv().is_ok() {}
        }
        let st_b = fleet.mgr("node-b")?.status(sess).ok_or_else(|| anyhow::anyhow!("missing status on partitioned B"))?;
        anyhow::ensure!(!st_b.is_leader, "isolated leader B must step down via check_quorum: {st_b:?}");
        let mut saw_lost = false;
        while let Ok(ev) = rx_b.try_recv() {
            if matches!(ev, FailoverEvent::LostLeadership { session, .. } if session == sess) {
                saw_lost = true;
            }
        }
        anyhow::ensure!(saw_lost, "isolated leader B must emit LostLeadership upon check_quorum step-down");
        Ok(())
    }

    /// Release Blocker #1 regression test: `propose_sync` waits for **true WAL + Raft quorum
    /// commit + state machine apply** (NOT in-memory queue admission) and returns `Err` when a
    /// partitioned leader steps down before achieving quorum commit!
    #[test]
    fn propose_sync_waits_for_quorum_commit_and_fails_on_step_down() -> anyhow::Result<()> {
        let fleet = Fleet::new_with_policy(&["node-a", "node-b", "node-c"], Policy { auto_failover: true, ..Policy::default() })?;
        let sess = "sess-quorum-sync";
        let open_row = Row::Session {
            session: sess.into(),
            info: Some(r#"{"id":"sess-quorum-sync","kind":"claude","target":"agent:node-b","epoch":1}"#.into()),
            meta: None,
            state: Some(r#"{"epoch":1}"#.into()),
            changes: None,
            epoch: Some(1),
            open: true,
            at: 1000,
        };
        fleet.store("node-b")?.put(vec![open_row.clone()]);
        fleet.open("node-b", sess, 1, vec![open_row])?;
        fleet.settle()?;
        anyhow::ensure!(fleet.mgr("node-b")?.status(sess).is_some_and(|s| s.is_leader && s.voters.len() == 3));

        // 1. Healthy quorum: `propose_sync` blocks until A/C replicate & commit, then returns `Ok(())`.
        let mgr_b = Arc::clone(fleet.mgr("node-b")?);
        let h_ok = thread::spawn(move || mgr_b.propose_sync(sess, 1, vec![user_event(sess, 1, "quorum-committed")]));
        // Pump messages across the fleet until `propose_sync` commits and returns!
        let mut ok_res = None;
        for _ in 0..64 {
            fleet.settle()?;
            if h_ok.is_finished() {
                ok_res = Some(h_ok.join().map_err(|_| anyhow::anyhow!("propose_sync thread panicked"))?);
                break;
            }
        }
        anyhow::ensure!(matches!(ok_res, Some(Ok(()))), "propose_sync must succeed once quorum commits: {ok_res:?}");

        // 2. Partition B from A and C: `propose_sync` MUST NOT return `Ok(())` while isolated, and
        //    MUST fail with `Err` when B steps down via `check_quorum`!
        let mgr_b2 = Arc::clone(fleet.mgr("node-b")?);
        let h_fail = thread::spawn(move || mgr_b2.propose_sync(sess, 1, vec![user_event(sess, 2, "uncommitted-while-isolated")]));
        // Drive B locally WITHOUT delivering to A/C -> entry sits uncommitted until `check_quorum` steps B down!
        // The first quorum check still counts answers from before the partition.
        fleet.detector_partition(&["node-b"],true)?;
        let quorum_ticks = raft_settings(node_id("node-b"), 1)?.core.election_tick;
        for _ in 0..quorum_ticks.saturating_mul(2) {
            fleet.mgr("node-b")?.drive_due();
            anyhow::ensure!(fleet.mgr("node-b")?.flush(TEST_WAIT));
            let out = fleet.mgr("node-b")?.outgoing();
            while out.try_recv().is_ok() {}
            if h_fail.is_finished() {
                break;
            }
        }
        let fail_res = h_fail.join().map_err(|_| anyhow::anyhow!("partitioned propose_sync thread panicked"))?;
        anyhow::ensure!(fail_res.is_err(), "propose_sync on partitioned leader MUST fail on step-down instead of returning Ok on queue admission: {fail_res:?}");
        Ok(())
    }

    /// Release Blocker #2 regression test (Commit-Before-Claim): an elected failover leader MUST NOT
    /// emit `FailoverEvent::Claimed` or advance `epoch` while partitioned before its ownership claim
    /// actually commits across quorum!
    #[test]
    fn failover_does_not_emit_claimed_until_ownership_claim_commits() -> anyhow::Result<()> {
        let fleet = Fleet::new_with_policy(&["node-a", "node-b", "node-c"], Policy { auto_failover: true, ..Policy::default() })?;
        let sess = "sess-commit-before-claim";
        let open_row = Row::Session {
            session: sess.into(),
            info: Some(r#"{"id":"sess-commit-before-claim","kind":"claude","target":"agent:node-b","epoch":1}"#.into()),
            meta: None,
            state: Some(r#"{"epoch":1}"#.into()),
            changes: None,
            epoch: Some(1),
            open: true,
            at: 1000,
        };
        fleet.store("node-b")?.put(vec![open_row.clone()]);
        fleet.open("node-b", sess, 1, vec![open_row])?;
        fleet.settle()?;
        anyhow::ensure!(fleet.mgr("node-b")?.status(sess).is_some_and(|s| s.is_leader && s.voters.len() == 3));

        // Kill B and route ONLY election vote messages between A and C, holding all `MsgAppend` so
        // the newly elected leader CANNOT commit its ownership claim yet!
        fleet.detector_partition(&["node-b"],false)?;
        let mut winner: Option<String> = None;
        let deadline=Instant::now()+TEST_WAIT+(TEST_WAIT / 2);
        while Instant::now()<deadline {
            fleet.advance(&["node-b"])?;
            for n in ["node-a", "node-c"] {
                anyhow::ensure!(fleet.mgr(n)?.flush(TEST_WAIT));
            }
            for from in ["node-a", "node-c"] {
                let rx = fleet.mgr(from)?.outgoing();
                while let Ok(msg) = rx.try_recv() {
                    if msg.to_name == "node-b" {
                        continue;
                    }
                    let is_vote = Message::decode(&msg.bytes).is_ok_and(|m| {
                        matches!(
                            m.msg_type,
                            MessageType::MsgRequestPreVote | MessageType::MsgRequestPreVoteResponse | MessageType::MsgRequestVote | MessageType::MsgRequestVoteResponse
                        )
                    });
                    if is_vote {
                        fleet.mgr(&msg.to_name)?.step(from, &msg.session, &msg.bytes)?;
                    }
                }
            }
            for n in ["node-a", "node-c"] {
                anyhow::ensure!(fleet.mgr(n)?.flush(TEST_WAIT));
                if fleet.mgr(n)?.status(sess).is_some_and(|s| s.is_leader) {
                    winner = Some(n.to_owned());
                    break;
                }
            }
            if winner.is_some() {
                break;
            }
        }
        let winner_name = winner.ok_or_else(|| anyhow::anyhow!("either node-a or node-c must win vote from survivor"))?;
        let st_uncommitted = fleet.mgr(&winner_name)?.status(sess).ok_or_else(|| anyhow::anyhow!("missing winner status"))?;
        anyhow::ensure!(st_uncommitted.is_leader && st_uncommitted.epoch == 1, "uncommitted winner must NOT advance epoch yet: {st_uncommitted:?}");
        anyhow::ensure!(
            fleet.mgr(&winner_name)?.failover_events().try_recv().is_err(),
            "elected failover leader MUST NOT emit FailoverEvent::Claimed before its ownership claim commits!"
        );

        // Now restore `MsgAppend` delivery between {A, C}: claim commits and `Claimed` fires!
        fleet.settle_except(&["node-b"])?;
        let mut saw_claimed = false;
        let rx_w = fleet.mgr(&winner_name)?.failover_events();
        while let Ok(ev) = rx_w.try_recv() {
            if let FailoverEvent::Claimed { session, new_epoch, previous_leader, .. } = ev
                && session == sess
                && new_epoch == 2
                && previous_leader == "node-b"
            {
                saw_claimed = true;
            }
        }
        anyhow::ensure!(saw_claimed, "FailoverEvent::Claimed must fire ONLY once the epoch-2 claim commits across quorum");
        anyhow::ensure!(fleet.mgr(&winner_name)?.status(sess).is_some_and(|s| s.is_leader && s.epoch == 2));
        Ok(())
    }

    /// Verifies that `session_inbox_rows` replicates BOTH live `inbox-pending` rows AND `value: None`
    /// delete tombstones so a delivered message is REMOVED on followers (never re-delivered on failover!).
    #[test]
    fn inbox_tombstone_deletes_replicated_pending_row_on_follower() -> anyhow::Result<()> {
        let fleet = Fleet::new(&["laptop-a", "laptop-b"])?;
        let sess = "sess-inbox-tombstone";
        let open_row = Row::Session {
            session: sess.into(),
            info: Some(r#"{"id":"sess-inbox-tombstone","target":"agent:laptop-b"}"#.into()),
            meta: None,
            state: None,
            changes: None,
            epoch: Some(1),
            open: true,
            at: 1000,
        };
        let pending_key = format!("{sess}/m-delivered-1").into_bytes();
        let pending_put = Row::Inbox(Box::new(InboxRow {
            session: sess.into(),
            family: "inbox-pending".into(),
            key: pending_key.clone(),
            value: Some(br#"{"text":"hello once"}"#.to_vec()),
        }));
        fleet.store("laptop-b")?.put(vec![open_row.clone(), pending_put]);
        fleet.store("laptop-b")?.sync()?;
        let rows_1 = session_inbox_rows(fleet.store("laptop-b")?, sess)?;
        fleet.open("laptop-b", sess, 1, vec![open_row])?;
        fleet.mgr("laptop-b")?.propose(sess, 1, rows_1)?;
        fleet.settle()?;

        fleet.store("laptop-a")?.sync()?;
        let before_del = fleet.store("laptop-a")?.inbox_rows("inbox-pending", format!("{sess}/").into_bytes())?;
        anyhow::ensure!(before_del.iter().any(|(k, _, _)| *k == pending_key), "follower must receive live inbox-pending row: {before_del:?}");

        // Delete the pending row locally (simulating delivery completion) and replicate via `session_inbox_rows`!
        let source = fleet.store("laptop-b")?.clone();
        source.enable_inbox_for("laptop-b")?;
        let key = pending_key.clone();
        crate::rt::block_on(async move {
            let delete = kiln_seams::records::Write::Delete { family: "inbox-pending".into(), key };
            source.inbox_commit(kiln_seams::records::Tx { conds: Vec::new(), writes: vec![delete] }, vec![sess.to_owned()], None).await
        })??;
        let rows_2 = session_inbox_rows(fleet.store("laptop-b")?, sess)?;
        anyhow::ensure!(
            rows_2.iter().any(|r| matches!(r, Row::Inbox(i) if i.key == pending_key && i.value.is_none())),
            "session_inbox_rows must include value=None delete tombstones: {rows_2:?}"
        );
        fleet.while_pumping(|| fleet.store("laptop-b")?.replicate_inbox(sess, 1, |rows| fleet.mgr("laptop-b")?.propose_sync(sess, 1, rows)))?;
        fleet.settle()?;

        fleet.store("laptop-a")?.sync()?;
        let after_del = fleet.store("laptop-a")?.inbox_rows("inbox-pending", format!("{sess}/").into_bytes())?;
        anyhow::ensure!(
            !after_del.iter().any(|(k, _, _)| *k == pending_key),
            "replicated delete tombstone MUST remove delivered inbox-pending row on follower: {after_del:?}"
        );
        Ok(())
    }

    #[test]
    fn cp_t32_fenced_legacy_replica_repairs_compacted_projection_from_authenticated_leader() -> anyhow::Result<()> {
        let mut fleet = Fleet::new(&["voter", "replica"])?;
        let session = "sess-legacy-peer-repair";
        fleet.open("voter", session, 1, vec![user_event(session, 1, "survives")])?;
        fleet.settle()?;
        for node in ["voter", "replica"] { fleet.mgr(node)?.force_compact(session, 0)?; }
        fleet.settle()?;

        let (mgr, db, db_dir, wal_dir) = fleet.nodes.get_mut("replica").context("replica node")?;
        mgr.shutdown();
        let checkpoint = wal_dir.path().join("state").join(format!("{:032x}.json",group_id(session)));
        let mut saved: SavedConvo = serde_json::from_slice(&std::fs::read(&checkpoint)?)?;
        saved.recovery_image = None;
        saved.projected = None;
        save_atomic(&checkpoint, &serde_json::to_vec(&saved)?)?;
        let (fresh_dir, fresh) = crate::convo::store::scratch()?;
        *mgr = Manager::open("replica", wal_dir.path(), Some(fresh.clone()))?;
        *db = fresh;
        *db_dir = fresh_dir;
        mgr.register_peers(["voter"]);
        mgr.mark_peer_v2("voter");
        anyhow::ensure!(mgr.status(session).is_some_and(|s| s.recovering && s.owner.is_none()));

        let deadline = Instant::now() + TEST_WAIT + (TEST_WAIT / 2);
        while fleet.mgr("replica")?.status(session).is_none_or(|s| s.recovering) && Instant::now() < deadline {
            for (from,(mgr,_,_,_)) in &fleet.nodes {
                mgr.drive_due();
                let _ = mgr.flush(TEST_POLL);
                let rx = mgr.outgoing();
                while let Ok(msg) = rx.try_recv() { fleet.mgr(&msg.to_name)?.step_owned(from, &msg.session, msg.bytes)?; }
            }
            thread::sleep(TEST_POLL);
        }
        fleet.settle()?;
        anyhow::ensure!(fleet.store("replica")?.kept(session, 8)?.is_some_and(|s| s.events.iter().any(|e| e.text == "survives")));
        Ok(())
    }

    /// Rolling-upgrade compatibility test: a peer that did NOT advertise `CONVO_RAFT_V2` is NEVER
    /// sent undecodable `KRC1` chunked wire frames or `WireEntry::Part` oversized WAL entries.
    #[test]
    fn legacy_peer_without_v2_is_not_sent_krc1_frames() -> anyhow::Result<()> {
        let (_sd_b, store_b) = crate::convo::store::scratch()?;
        let wal_b = tempfile::tempdir()?;
        let mgr_b = Manager::open("laptop-b", wal_b.path(), Some(store_b.clone()))?;

        let (_sd_leg, store_leg) = crate::convo::store::scratch()?;
        let wal_leg = tempfile::tempdir()?;
        let mgr_leg = Manager::open("laptop-legacy", wal_leg.path(), Some(store_leg.clone()))?;

        // Deliberately DO NOT call `mgr_b.mark_peer_v2("laptop-legacy")`!
        mgr_b.register_peers(["laptop-legacy"]);
        mgr_leg.register_peers(["laptop-b"]);

        let sess = "sess-legacy-compat";
        let open_row = Row::Session {
            session: sess.into(),
            info: Some(r#"{"id":"sess-legacy-compat","target":"agent:laptop-b"}"#.into()),
            meta: None,
            state: None,
            changes: None,
            epoch: Some(1),
            open: true,
            at: 1000,
        };
        store_b.put(vec![open_row.clone()]);
        let founder = mgr_b.clone();
        let opening = thread::spawn(move || founder.open_leader(sess, 1, vec![open_row]));

        // Pump until the legacy replica confirms the founding entry.
        let deadline = Instant::now() + TEST_WAIT;
        while !opening.is_finished() && Instant::now() < deadline {
            anyhow::ensure!(mgr_b.flush(TEST_WAIT) && mgr_leg.flush(TEST_WAIT));
            let rx_b = mgr_b.outgoing();
            while let Ok(m) = rx_b.try_recv() {
                anyhow::ensure!(m.bytes.get(..4) != Some(WIRE_CHUNK_MAGIC.as_slice()), "must never send KRC1 frame to legacy peer");
                mgr_leg.step("laptop-b", &m.session, &m.bytes)?;
            }
            let rx_l = mgr_leg.outgoing();
            while let Ok(m) = rx_l.try_recv() {
                mgr_b.step("laptop-legacy", &m.session, &m.bytes)?;
            }
            mgr_b.drive_due();
            mgr_leg.drive_due();
        }
        opening.join().map_err(|_| anyhow::anyhow!("legacy founder stopped"))??;
        anyhow::ensure!(mgr_b.status(sess).is_some_and(|s| s.learners.contains(&node_id("laptop-legacy"))), "legacy peer must join via v1 framing");

        // 1. Oversized single row requiring `WireEntry::Part` MUST be refused cleanly rather than
        //    fencing the legacy peer with undecodable `WireEntry::Part` entries!
        let corrupt_before = corrupt_entries();
        let huge = user_event(sess, 1, &"Q".repeat(1200 * 1024));
        let part_res = mgr_b.propose_sync(sess, 1, vec![user_event(sess, 501, "before"), huge, user_event(sess, 502, "after")]);
        anyhow::ensure!(part_res.is_err(), "proposing WireEntry::Part with legacy peer in group must fail explicitly: {part_res:?}");
        anyhow::ensure!(corrupt_entries() == corrupt_before, "legacy peer must suffer zero corrupt_entries");
        anyhow::ensure!(store_b.kept(sess, 20)?.is_some_and(|k| k.events.iter().all(|e| e.text != "before" && e.text != "after")), "rejecting a mixed operation must project no prefix or suffix");

        // 2. Oversized snapshot (> WIRE_PART_MAX) for a late legacy peer MUST NOT emit KRC1 frames!
        let pad = "p".repeat(1800);
        for seq in 1..=320u64 {
            let ev = user_event(sess, seq, &format!("m-{seq}-{pad}"));
            store_b.put(vec![ev.clone()]);
            mgr_b.propose(sess, 1, vec![ev])?;
        }
        anyhow::ensure!(mgr_b.flush(TEST_WAIT));
        mgr_b.force_compact(sess, 0)?;
        anyhow::ensure!(mgr_b.flush(TEST_WAIT));

        // Register a second late legacy peer without `mark_peer_v2` and confirm zero KRC1 frames!
        mgr_b.register_peers(["laptop-legacy", "laptop-legacy-late"]);
        for _ in 0..8 {
            mgr_b.drive_due();
            anyhow::ensure!(mgr_b.flush(TEST_WAIT));
            let rx_b = mgr_b.outgoing();
            while let Ok(m) = rx_b.try_recv() {
                anyhow::ensure!(
                    m.bytes.get(..4) != Some(WIRE_CHUNK_MAGIC.as_slice()),
                    "must NEVER emit KRC1 chunked snapshot frame to non-v2 peer {}",
                    m.to_name
                );
            }
        }
        Ok(())
    }
}
