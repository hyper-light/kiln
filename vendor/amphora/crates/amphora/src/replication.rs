//! Amphora's resolved-page state machine for hyper-durable (U15, CP-T1.8).
//!
//! The designated local owner evaluates SQL once after a quorum read fence, including for
//! follower-origin requests, then rolls back its speculative transaction after capture. Only
//! resolved page deltas enter Raft: defaults, random functions and adaptive reads never run again
//! during application. Applied index, term and membership share the deltas' atomic WAL commit; the
//! owner's flushing thread flushes that log behind the owner, and copies it into the database's
//! file (replication.md, "A replica's database log"). The product Database actor drives
//! ReadIndex, elections and durability through hyper-durable, which owns quorum decisions, commit
//! fencing, recovery and snapshots; its GroupStore uses hyper-log. The host only carries
//! authenticated replication messages.

use std::path::Path;

use amphora_api::{self as api, Refusal};
use amphora_engine::Value;
use hyper_durable::{EntryRef, Fatal, Point, StateMachine};
use hyper_raft::proto::{ConfChangeV2, ConfState};
use hyper_raft::wire::Record;
use serde::{Deserialize, Serialize};

use crate::db::Db;

pub use crate::database::replicated::{ReceiveReceipt, ReplicaHandle};
pub mod bootstrap;
pub(crate) mod class;
pub(crate) mod closed;
pub(crate) mod floor;
pub(crate) mod readsets;

const BASELINE_DELTA: &[u8] = b"amphora.delta.v1\0";
const BASELINE_SNAPSHOT: &[u8] = b"amphora.snapshot.v1\0";
#[derive(Serialize, Deserialize)]
struct BaselineSnapshot {
    declaration: api::replication::Bootstrap,
    pages: Vec<(u32, Vec<u8>)>,
}

const META: &str = "CREATE TABLE IF NOT EXISTS __amphora_replica (id INTEGER PRIMARY KEY CHECK(id = 1), applied TEXT NOT NULL, term TEXT NOT NULL, configuration BLOB NOT NULL)";
/// One resolved transaction's page budget. Oversize transactions are refused before proposal.
pub const COMMAND_BYTES: usize = api::REQUEST_MAX;
/// The room for one image now: the process memory budget's room for an image (the seam owner's
/// ruling (c): an image is held whole in memory until snapshots stream, so the memory it may take
/// bounds it; docs/design/replication.md, "Decision (lane constants): an image is bounded by the
/// memory budget's room for one"; SB.1). Read again at every image made or installed; refused,
/// typed, where the process's memory can't be read.
pub(crate) fn image_room() -> Result<usize, Refusal> {
    crate::memory::process().image_room()
}
/// Format: a database page's bytes, the only page size `validate_pages` admits.
const PAGE_BYTES: usize = 4096;
/// Format (postcard's wire format, "Varint encoded integers"): a `u64` or `usize` takes at most
/// ten bytes, a `u32` at most five, seven bits a byte.
const U64_MOST: usize = 10;
const U32_MOST: usize = 5;
/// The bytes postcard takes for the varint `n`.
const fn varint_bytes(n: usize) -> usize {
    let (mut rest, mut bytes) = (n, 1usize);
    while let Some(next) = rest.checked_div(128) {
        if next == 0 {
            break;
        }
        rest = next;
        bytes = bytes.saturating_add(1);
    }
    bytes
}
/// The most bytes a declared baseline's encoding takes: its source node's varint and its 32-byte
/// identity, which postcard writes without a length (`api::replication::Bootstrap`).
const BOOTSTRAP_MOST: usize = U64_MOST.saturating_add(32);
/// The bytes an encoding of `budget` bytes of pages takes past the pages themselves (replication.md,
/// "Decision (lane constants): an envelope is what its encoding adds"; EN.1): the list's length,
/// and each page's id and length, at most `budget / PAGE_BYTES` pages.
const fn pages_envelope(budget: usize) -> usize {
    let pages = match budget.checked_div(PAGE_BYTES) {
        Some(pages) => pages,
        None => 0,
    };
    let page = U32_MOST.saturating_add(varint_bytes(PAGE_BYTES));
    U64_MOST.saturating_add(pages.saturating_mul(page))
}
/// What a prepared transaction's encoding adds to its pages, at the most: the baseline's prefix and
/// declaration, its base's index and term, and its pages' envelope. A read set rides in what an
/// entry leaves of it and of the pages' budget, or goes without one (`Prepared::encode`).
const COMMAND_ENVELOPE: usize = BASELINE_DELTA
    .len()
    .saturating_add(BOOTSTRAP_MOST)
    .saturating_add(U64_MOST.saturating_mul(2))
    .saturating_add(pages_envelope(COMMAND_BYTES));
/// What an image's encoding adds to its `budget` bytes of pages, at the most: the baseline's prefix
/// and declaration, and its pages' envelope.
const fn image_envelope(budget: usize) -> usize {
    BASELINE_SNAPSHOT
        .len()
        .saturating_add(BOOTSTRAP_MOST)
        .saturating_add(pages_envelope(budget))
}
/// What the filesystem holding a machine's database has free for this process, in bytes: read on
/// its owner's flushing thread after each checkpoint pass (replication.md, "A replica's database
/// log bounded by what it measures").
pub(crate) type FreeSpace = Box<dyn FnMut() -> std::io::Result<u64> + Send>;

#[derive(Debug, Serialize, Deserialize)]
pub struct Prepared {
    base_index: u64,
    base_term: u64,
    pages: Vec<(u32, Vec<u8>)>,
    #[serde(skip)]
    baseline: Option<api::replication::Bootstrap>,
    /// What its session read, carried after its encoding (`readsets`): `None` for a session that
    /// read more than its entry could carry, and for what isn't a caller's write.
    #[serde(skip)]
    reads: Option<readsets::ReadSet>,
}

impl Prepared {
    #[cfg(test)]
    pub(crate) fn for_test(
        base: Point,
        pages: Vec<(u32, Vec<u8>)>,
        reads: Option<readsets::ReadSet>,
    ) -> Self {
        Self {
            base_index: base.index,
            base_term: base.term,
            pages,
            baseline: None,
            reads,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    pub fn encode(&self) -> Result<Vec<u8>, Refusal> {
        let mut bytes = if let Some(declaration) = &self.baseline {
            let mut bytes = BASELINE_DELTA.to_vec();
            bytes.extend(postcard::to_allocvec(&(declaration, self)).map_err(unavailable)?);
            bytes
        } else {
            postcard::to_allocvec(self).map_err(unavailable)?
        };
        let bound = COMMAND_BYTES.saturating_add(COMMAND_ENVELOPE);
        api::bound("replicated command", bytes.len(), bound)?;
        // The read set rides after the encoding while the entry keeps its bound; past it, the
        // write goes without one and the exact base decides it, as for a binary from before.
        if let Some(reads) = &self.reads {
            let trailer = postcard::to_allocvec(reads).map_err(unavailable)?;
            if bytes.len().saturating_add(trailer.len()) <= bound {
                bytes.extend(trailer);
            }
        }
        Ok(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self, Refusal> {
        api::bound(
            "replicated command",
            bytes.len(),
            COMMAND_BYTES.saturating_add(COMMAND_ENVELOPE),
        )?;
        let prepared: Self = if let Some(payload) = bytes.strip_prefix(BASELINE_DELTA) {
            let (declaration, mut prepared): (api::replication::Bootstrap, Self) =
                postcard::from_bytes(payload).map_err(unavailable)?;
            prepared.baseline = Some(declaration);
            prepared
        } else {
            postcard::from_bytes(bytes).map_err(unavailable)?
        };
        validate_pages(&prepared.pages, COMMAND_BYTES)?;
        Ok(prepared)
    }
}

/// A prepared transaction's base, the fields its encoding starts with: read without its pages.
#[derive(Deserialize)]
struct Base {
    base_index: u64,
    base_term: u64,
}

/// The base fence: a prepared transaction applies only on the point it was prepared at. Every
/// entry applied moves the point, so the entry before it in the log decides.
fn on_base(base_index: u64, base_term: u64, point: Point) -> Result<(), Refusal> {
    if base_index == point.index && base_term == point.term {
        Ok(())
    } else {
        Err(api::invalid(
            "prepared transaction",
            "the database advanced since preparation",
        ))
    }
}

/// What applying the prepared transaction `data` will answer, the entry before it in the log being
/// `before`: decided from the log, so that a follower in the replicated class answers on its
/// group's commit (replication.md, "A write made at a follower is answered on the leader's
/// commit"). The outer error is a malformed entry, whose apply fences.
pub(crate) fn verdict(data: &[u8], before: Point) -> Result<Result<(), Refusal>, Refusal> {
    let (_, data) = closed::open(data)?;
    api::bound(
        "replicated command",
        data.len(),
        COMMAND_BYTES.saturating_add(COMMAND_ENVELOPE),
    )?;
    let base: Base = if let Some(payload) = data.strip_prefix(BASELINE_DELTA) {
        let (_, base): (api::replication::Bootstrap, Base) =
            postcard::from_bytes(payload).map_err(unavailable)?;
        base
    } else {
        postcard::from_bytes(data).map_err(unavailable)?
    };
    Ok(on_base(base.base_index, base.base_term, before))
}

/// A local apply result is released only once its entry is committed: in the persisted class once
/// this member's raft log holds it durably, so a restart that finds the database's log behind it
/// applies it again from the raft log (durable.md §4.1, §4.3).
#[derive(Debug)]
pub struct Applied {
    pub index: u64,
    pub context: Vec<u8>,
    pub result: Result<(), Refusal>,
}

/// Owned exclusively by one hyper-durable Replica. It has no application-side writer alongside it.
pub struct Machine {
    db: Db,
    /// The last entry applied.
    point: Point,
    /// The last entry a finished flush of the database's log covers: what a restart opens with
    /// applied ([`StateMachine::durable`]). Behind `point` only in the replicated class.
    flushed: Point,
    /// The replicated class (docs/design/replication.md, "Fast acknowledgement"): an ordinary entry
    /// is applied without a flush of its own, and the owner flushes the log behind it.
    replicated: bool,
    configuration: ConfState,
    baseline: bootstrap::Baseline,
    /// The group's time at the last entry applied (replication.md, "Follower reads at a closed
    /// time"): none before its first stamped entry.
    time: Option<u64>,
    // A test's count of the entries whose stamp was lifted past the time before them.
    #[cfg(test)]
    lifted: u64,
    /// The group's class as its log recorded it last (replication.md, "The class stated once per
    /// group"), and the class this member's owner stated: the two must stay compatible.
    class: Option<api::replication::Durability>,
    stated: Option<api::replication::Durability>,
    // A test's count of the class entries this machine applied that changed its record.
    #[cfg(test)]
    class_changes: u64,
    // The most an image of this machine's carrier holds (its owner's transport, or a test's lower
    // bound): usize::MAX where nothing carries it but memory. The image's room is the memory
    // budget's room for one, no more than this (SB.1).
    image_cap: usize,
    // A test's stand-in for the filesystem's free bytes, read in place of the filesystem's own.
    #[cfg(test)]
    free_bytes: std::sync::Arc<std::sync::Mutex<Option<u64>>>,
    // A test's stand-in for the filesystem's number, for a database on a simulated disk.
    #[cfg(test)]
    filesystem: Option<u64>,
    // The index of the last configuration change applied, or more: the change's own index when
    // this machine applied it, the applied point when it opened or installed an image. The floor's
    // proof counts an attestation before marker J only if it was made at or after it, so an
    // overestimate only delays J (replication.md, "The upgrade that introduces the join rows").
    configuration_at: u64,
    // A test's stand-in for a member running a binary from before the floor: it writes no join
    // rows, as such a binary doesn't.
    #[cfg(test)]
    floor_aware: bool,
    // A test's record of this member's image digest at each applied index.
    #[cfg(test)]
    digests: Option<Vec<(u64, u64)>>,
    // A test's hold on this machine's flushes of its database's log.
    #[cfg(test)]
    flush_gate: std::sync::Arc<FlushGate>,
    // The bytes of the image this machine opened with, its pages as `image` writes them: what the
    // compaction rule weighs a log never compacted against (`StateMachine::image_bytes`; CT.1).
    opened_bytes: u64,
    // The bytes of the last image made, and of every entry applied since the machine opened: what
    // the owner times its images and its log's replay by (`database::compacting`; CT.1).
    imaged_bytes: u64,
    applied_bytes: u64,
    _owner: crate::database::Owned,
}

/// A test's hold on a machine's flushes: while held, a flush waits for the test to open it, unless
/// it runs on the thread the test holds it from (the owner's, in a driven test), where it counts
/// that it got there and goes on, so that a test finds an owner that would stand behind it.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct FlushGate {
    state: std::sync::Mutex<GateState>,
    opened: std::sync::Condvar,
}
#[cfg(test)]
#[derive(Default)]
struct GateState {
    held: bool,
    from: Option<std::thread::ThreadId>,
    reached_from: u64,
    // Every flush from now fails as a full disk's does.
    full: bool,
}
#[cfg(test)]
impl FlushGate {
    /// Holds every flush from now, from this thread.
    pub(crate) fn hold(&self) {
        let mut state = self.state.lock().unwrap();
        state.held = true;
        state.from = Some(std::thread::current().id());
    }
    pub(crate) fn open(&self) {
        self.state.lock().unwrap().held = false;
        self.opened.notify_all();
    }
    /// Fails every flush from now with the error a full disk gives (`ENOSPC`).
    pub(crate) fn fill(&self) {
        self.state.lock().unwrap().full = true;
    }
    /// Flushes that reached the hold on the thread it was held from.
    pub(crate) fn reached_from_holder(&self) -> u64 {
        self.state.lock().unwrap().reached_from
    }
    fn pass(&self) -> std::io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.held && state.from == Some(std::thread::current().id()) {
            state.reached_from += 1;
            return Ok(());
        }
        while state.held {
            state = self.opened.wait(state).unwrap();
        }
        if state.full {
            return Err(std::io::Error::from(std::io::ErrorKind::StorageFull));
        }
        Ok(())
    }
}

/// A floor this group's members didn't prove never enters replicated mode: one a standalone
/// owner installed, held where joins aren't tracked, is refused at open, at the seed and at an
/// image's install (replication.md, "Raising the floor", d). A floor raised on proof always has
/// joins tracked.
fn unproven_floor(db: &Db) -> anyhow::Result<()> {
    if crate::database::installed_floor(db)?.is_some() && floor::tracked(db)?.is_none() {
        return Err(Refusal::Fenced(
            "a floor not proven by this group's members; raise it again once joins are tracked"
                .into(),
        )
        .into());
    }
    Ok(())
}

fn unavailable(e: impl std::fmt::Display) -> Refusal {
    Refusal::Unavailable(e.to_string())
}
fn failed(e: impl std::fmt::Display) -> Fatal {
    tracing::error!(target: "amphora.replication", event = "machine_failed", error = %e, "replica must reopen from durable state");
    Fatal("Amphora state machine failed; see amphora.replication event")
}
fn text(n: u64) -> Value {
    Value::from_text(n.to_string())
}
fn validate_pages(pages: &[(u32, Vec<u8>)], bound: usize) -> Result<(), Refusal> {
    let mut bytes = 0usize;
    let mut previous = 0;
    for (id, page) in pages {
        if *id <= previous || page.len() != PAGE_BYTES {
            return Err(api::invalid(
                "replicated pages",
                "page IDs must be sorted, unique and positive; pages must be 4096 bytes",
            ));
        }
        previous = *id;
        bytes = bytes
            .checked_add(page.len())
            .ok_or_else(|| api::invalid("replicated pages", "size overflow"))?;
    }
    api::bound("replicated page bytes", bytes, bound)
}
fn marker(db: &mut Db, point: Point, configuration: &ConfState) -> anyhow::Result<()> {
    let mut bytes = Vec::new();
    configuration.encode(&mut bytes);
    // Installing an image can already carry these exact values. Force a real page write:
    // imported raw frames have no commit marker, and an optimized no-op UPDATE must not leave
    // them as an unpublished WAL suffix while the machine reports the new durable point.
    db.query("DELETE FROM __amphora_replica WHERE id = 1", [], |_| {
        Ok(true)
    })?;
    db.query("INSERT INTO __amphora_replica(id, applied, term, configuration) VALUES (1, ?1, ?2, ?3) ON CONFLICT(id) DO UPDATE SET applied = excluded.applied, term = excluded.term, configuration = excluded.configuration", [text(point.index), text(point.term), Value::from_slice(&bytes)?], |_| Ok(true))
}

impl Machine {
    pub fn open(path: &Path, initial: ConfState) -> Result<Self, Refusal> {
        // The lease's descriptor is charged before the lease is opened.
        let descriptor = crate::descriptors::process()?.take("open descriptors", 1)?;
        let owner = crate::database::Owned::new(
            std::sync::Arc::new(crate::database::exclusive_file(path)?),
            descriptor,
            None,
        );
        Self::open_with_owner(path, initial, owner, 0, None, false)
    }

    pub(crate) fn open_with_owner(
        path: &Path,
        initial: ConfState,
        owner: crate::database::Owned,
        node_id: u64,
        bootstrap: Option<api::replication::Bootstrap>,
        replicated: bool,
    ) -> Result<Self, Refusal> {
        Self::open_db(
            |owner| crate::database::open_db(path, owner),
            initial,
            owner,
            node_id,
            bootstrap,
            replicated,
        )
    }

    /// [`Machine::open_with_owner`] on `io`: a test's disk, whose power the test cuts.
    #[cfg(test)]
    pub(crate) fn open_on(
        path: &Path,
        io: std::sync::Arc<dyn amphora_engine::IO>,
        initial: ConfState,
        owner: crate::database::Owned,
        node_id: u64,
        replicated: bool,
    ) -> Result<Self, Refusal> {
        Self::open_db(
            |_| Db::open_with(path, io).map_err(unavailable),
            initial,
            owner,
            node_id,
            None,
            replicated,
        )
    }

    fn open_db(
        open: impl FnOnce(&crate::database::Owned) -> Result<Db, Refusal>,
        initial: ConfState,
        owner: crate::database::Owned,
        node_id: u64,
        bootstrap: Option<api::replication::Bootstrap>,
        replicated: bool,
    ) -> Result<Self, Refusal> {
        if let Some(declaration) = &bootstrap
            && (node_id == 0 || !initial.voters.contains(&declaration.source_node))
        {
            return Err(api::invalid(
                "replica baseline",
                "the declared source must be an initial voter and the local node ID must be positive",
            ));
        }
        let mut db = open(&owner)?;
        db.hold_owner(owner.clone());
        // Check before retaining a baseline image or writing a replica marker.
        unproven_floor(&db).map_err(|e| crate::database::refused(&e))?;
        let mut marked = false;
        db.query(
            "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = '__amphora_replica'",
            [],
            |_| {
                marked = true;
                Ok(false)
            },
        )
        .map_err(unavailable)?;
        // Check unfinished legacy adoption before retaining any image or creating a marker.
        let mut restore_table = false;
        db.query(
            "SELECT 1 FROM sqlite_schema WHERE name = '__amphora_restore'",
            [],
            |_| {
                restore_table = true;
                Ok(false)
            },
        )
        .map_err(unavailable)?;
        if restore_table {
            let mut incomplete = false;
            db.query(
                "SELECT 1 FROM __amphora_restore WHERE done = 0 LIMIT 1",
                [],
                |_| {
                    incomplete = true;
                    Ok(false)
                },
            )
            .map_err(unavailable)?;
            if incomplete {
                return Err(api::invalid(
                    "replica baseline",
                    "complete the pinned restore before replicated startup",
                ));
            }
        }
        let baseline = bootstrap::Baseline::open(&mut db, node_id, bootstrap, marked)?;
        crate::database::bootstrap_db(&db)?;
        let mut incomplete = false;
        db.query("SELECT 1 FROM __amphora_restore WHERE done = 0", [], |_| {
            incomplete = true;
            Ok(false)
        })
        .map_err(unavailable)?;
        if incomplete {
            return Err(api::invalid(
                "replica baseline",
                "complete the pinned restore before replicated startup",
            ));
        }
        db.exec(META).map_err(unavailable)?;
        let mut state = None;
        db.query(
            "SELECT applied, term, configuration FROM __amphora_replica WHERE id = 1",
            [],
            |row| {
                let index = row.get::<String>(0).map_err(unavailable)?.parse()?;
                let term = row.get::<String>(1).map_err(unavailable)?.parse()?;
                let bytes = row
                    .get_value(2)
                    .to_blob()
                    .ok_or_else(|| anyhow::anyhow!("membership is not a blob"))?;
                state = Some((Point { index, term }, ConfState::decode(bytes)?));
                Ok(false)
            },
        )
        .map_err(unavailable)?;
        if marked && state.is_none() {
            return Err(Refusal::Fenced(
                "replica table lacks its durable applied marker".into(),
            ));
        }
        let (point, configuration) = state.unwrap_or((Point::default(), initial));
        let recorded = class::recorded(&db).map_err(unavailable)?;
        let time = closed::recorded(&db).map_err(unavailable)?;
        db.transaction(|db| marker(db, point, &configuration))
            .map_err(unavailable)?;
        db.flusher()().map_err(unavailable)?;
        let opened_bytes = db.image_bytes().map_err(unavailable)?;
        let opened_bytes = u64::try_from(opened_bytes).unwrap_or(u64::MAX);
        Ok(Self {
            db,
            point,
            flushed: point,
            replicated,
            configuration,
            baseline,
            time,
            #[cfg(test)]
            lifted: 0,
            class: recorded,
            stated: None,
            #[cfg(test)]
            class_changes: 0,
            image_cap: usize::MAX,
            #[cfg(test)]
            free_bytes: std::sync::Arc::default(),
            #[cfg(test)]
            filesystem: None,
            configuration_at: point.index,
            #[cfg(test)]
            floor_aware: true,
            #[cfg(test)]
            digests: None,
            #[cfg(test)]
            flush_gate: std::sync::Arc::default(),
            opened_bytes,
            imaged_bytes: 0,
            applied_bytes: 0,
            _owner: owner,
        })
    }

    /// A member that joins catches up by this machine's image once the log no longer reaches
    /// back, and an image is bounded (replication.md, "Membership"): past its ceiling, a joiner
    /// is refused before it is added, never left unable to catch up.
    /// Whether an image of the database now is within this machine's image budget: past it,
    /// `image` refuses, and a compaction, which needs one, waits (CT.1).
    pub(crate) fn image_fits(&mut self) -> Result<bool, Refusal> {
        let size = self.db.image_bytes().map_err(unavailable)?;
        Ok(size <= self.snapshot_bytes()?)
    }
    /// The bytes of the last image this machine made (CT.1).
    pub(crate) fn imaged_bytes(&self) -> u64 {
        self.imaged_bytes
    }
    /// The bytes of every entry this machine applied since it opened, as the core counts an
    /// entry's (CT.1).
    pub(crate) fn applied_bytes(&self) -> u64 {
        self.applied_bytes
    }

    pub(crate) fn admits_joiner(&mut self) -> Result<(), Refusal> {
        let size = self.db.image_bytes().map_err(unavailable)?;
        api::bound(
            "database a joiner installs as a snapshot",
            size,
            self.snapshot_bytes()?,
        )
    }
    /// Makes the filesystem's free bytes read `bytes` from now, as its owner's flushing thread
    /// reads them.
    #[cfg(test)]
    pub(crate) fn set_free_bytes(&self, bytes: u64) {
        *self.free_bytes.lock().unwrap() = Some(bytes);
    }
    /// Bounds this machine's images to `bytes` beside the memory budget's room: its carrier's
    /// most (the replicated owner's transport), or a test's.
    pub(crate) fn set_snapshot_bytes(&mut self, bytes: usize) {
        self.image_cap = bytes;
    }
    /// The most an image of this machine may take now: the memory budget's room for one, no more
    /// than its carrier holds (SB.1).
    fn snapshot_bytes(&self) -> Result<usize, Refusal> {
        Ok(image_room()?.min(self.image_cap))
    }

    /// The class this member's owner states, held to the group's record from now on: refused,
    /// typed, if the database records a class it isn't compatible with (replication.md, "The
    /// class stated once per group").
    pub(crate) fn state_class(
        &mut self,
        durability: &api::replication::Durability,
    ) -> Result<(), Refusal> {
        let stated = class::canonical(durability);
        if let Some(record) = &self.class
            && !class::compatible(&stated, record)
        {
            return Err(api::invalid(
                "replicated class",
                "the group's log states another class than this member's",
            ));
        }
        self.stated = Some(stated);
        Ok(())
    }
    /// The group's class as this machine recorded it last.
    #[cfg(test)]
    pub(crate) fn class(&self) -> Option<&api::replication::Durability> {
        self.class.as_ref()
    }
    #[cfg(test)]
    pub(crate) fn class_changes(&self) -> u64 {
        self.class_changes
    }
    /// Whether this member may count held answers: every fate it states for a member its
    /// configuration names is the record's. Compatible alone isn't enough: two members stating
    /// different fates for a joiner not yet recorded would each count by their own.
    pub(crate) fn class_agreed(&self) -> bool {
        let (Some(stated), Some(record)) = (&self.stated, &self.class) else {
            return false;
        };
        class::compatible(stated, record) && class::recorded_all(record, stated, &self.named())
    }
    /// The members this machine's configuration names, voters and learners, both halves.
    fn named(&self) -> Vec<u64> {
        let conf = &self.configuration;
        conf.voters
            .iter()
            .chain(&conf.learners)
            .chain(&conf.voters_outgoing)
            .chain(&conf.learners_next)
            .copied()
            .collect()
    }
    /// The class entry this member would propose leading: the group's record joined with its own
    /// class, to the members its configuration names, when that changes the record
    /// (replication.md, "The class stated once per group", the amendment). The record only grows
    /// between changes of members, whichever compatible member leads.
    pub(crate) fn class_proposal(&self) -> Option<api::replication::Durability> {
        let stated = self.stated.as_ref()?;
        let named = self.named();
        let proposal = match &self.class {
            Some(record) if !class::compatible(record, stated) => return None,
            record => class::joined(record.as_ref(), stated, &named),
        };
        if self.class.as_ref() == Some(&proposal) {
            None
        } else {
            Some(proposal)
        }
    }
    /// The class this member's owner stated, against the group's record: a member whose class
    /// isn't compatible with it stops (replication.md, "The class stated once per group").
    fn hold_to_class(&self) -> Result<(), Fatal> {
        if let (Some(stated), Some(record)) = (&self.stated, &self.class)
            && !class::compatible(stated, record)
        {
            tracing::error!(target: "amphora.replication", event = "class_refused", stated = ?stated, recorded = ?record);
            return Err(Fatal(
                "the group's log states another replicated class than this member's",
            ));
        }
        Ok(())
    }

    /// The last configuration change's index as this machine knows it, or more.
    pub(crate) fn configuration_at(&self) -> u64 {
        self.configuration_at
    }
    /// What opening this machine again does to `configuration_at`: it starts at the applied
    /// point, as the machine no longer knows the last change's own index.
    #[cfg(test)]
    pub(crate) fn reopen_configuration_at(&mut self) {
        self.configuration_at = self.point.index;
    }
    #[cfg(test)]
    pub(crate) fn set_floor_aware(&mut self, aware: bool) {
        self.floor_aware = aware;
    }
    #[cfg(test)]
    fn writes_joins(&self) -> bool {
        self.floor_aware
    }
    #[cfg(not(test))]
    fn writes_joins(&self) -> bool {
        true
    }
    #[cfg(test)]
    pub(crate) fn record_digests(&mut self) {
        self.digests = Some(Vec::new());
    }
    #[cfg(test)]
    pub(crate) fn digests(&self) -> &[(u64, u64)] {
        self.digests.as_deref().unwrap_or_default()
    }
    #[cfg(test)]
    fn record_digest(&mut self, at: u64) -> Result<(), Fatal> {
        use std::hash::{Hash, Hasher};
        if self.digests.is_none() {
            return Ok(());
        }
        let room = self.snapshot_bytes().map_err(failed)?;
        let pages = self.db.image_pages(room).map_err(failed)?;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for (id, page) in &pages {
            // A page's last eight bytes are the engine's checksum of the rest, written when the
            // page is written out (engine storage/checksum.rs); whether a read sees it before or
            // after that is timing, not content. The digest is of the content.
            id.hash(&mut hasher);
            page.get(..page.len().saturating_sub(8)).hash(&mut hasher);
        }
        if let Some(digests) = self.digests.as_mut() {
            digests.push((at, hasher.finish()));
        }
        Ok(())
    }

    /// Whether this machine applies in the replicated class.
    pub(crate) fn is_replicated(&self) -> bool {
        self.replicated
    }
    pub(crate) fn bootstrap_pending(&self) -> bool {
        self.baseline.pending()
    }
    pub(crate) fn bootstrap_source(&self) -> Option<u64> {
        self.baseline.source()
    }
    pub(crate) fn prepare_bootstrap(&self, entry_room: usize) -> Result<Option<Vec<u8>>, Refusal> {
        unproven_floor(&self.db).map_err(|e| crate::database::refused(&e))?;
        self.baseline.proposal(entry_room)
    }
    fn require_baseline(&self) -> Result<(), Refusal> {
        if self.baseline.pending() {
            return Err(api::invalid(
                "replica baseline",
                "initial image has not committed; canonical operations are withheld",
            ));
        }
        Ok(())
    }

    pub(crate) fn prepare_db<R>(
        &mut self,
        f: impl FnOnce(&mut Db) -> anyhow::Result<R>,
    ) -> Result<(Prepared, R), Refusal> {
        self.require_baseline()?;
        // Each page number takes at least one byte of the entry, so a session that read more
        // pages than the entry has bytes could not carry its read set.
        let bound =
            u64::try_from(COMMAND_BYTES.saturating_add(COMMAND_ENVELOPE)).unwrap_or(u64::MAX);
        let (answer, pages, traced) = self
            .db
            .prepare_traced(COMMAND_BYTES, bound, f)
            .map_err(|e| crate::database::refused(&e))?;
        validate_pages(&pages, COMMAND_BYTES)?;
        let page1 = match pages.iter().find(|(page, _)| *page == 1) {
            Some((_, image)) => readsets::page1_change(&traced.page_one, image),
            None => readsets::Page1::Unwritten,
        };
        Ok((
            Prepared {
                base_index: self.point.index,
                base_term: self.point.term,
                pages,
                baseline: self.baseline.declaration().cloned(),
                // Only on a base whose state has the closed time's table: an apply that makes it
                // (the group's first stamped entry) changes the schema beyond any entry's pages,
                // so a write prepared before it must not move past it (`readsets`).
                reads: traced
                    .reads
                    .filter(|_| self.time.is_some())
                    .map(|reads| readsets::ReadSet { reads, page1 }),
            },
            answer,
        ))
    }

    pub(crate) fn prepare_compaction(
        &mut self,
        namespace: &str,
        before: api::Rev,
        until: std::time::Instant,
        room: usize,
    ) -> Result<(Prepared, api::CompactionProgress), Refusal> {
        use crate::database::compaction;
        self.require_baseline()?;
        let room = room.min(COMMAND_BYTES);
        let pages = compaction::page_limit(room)?;
        let (answer, image) = compaction::retry(|visits| {
            let result = self.db.compaction_attempt(pages, true, |db| {
                let _deadline = db.deadline(until);
                compaction::ready(db, namespace, until, false)?;
                compaction::step(db, namespace, before, visits)
            });
            compaction::deadline_result(result, until)
        })?;
        validate_pages(&image, COMMAND_BYTES)?;
        let prepared = Prepared {
            base_index: self.point.index,
            base_term: self.point.term,
            pages: image,
            baseline: self.baseline.declaration().cloned(),
            reads: None,
        };
        // The hard page fence proves the worst-case bound; retain the exact serialization check
        // before proposal as a format-change and arithmetic guard.
        api::bound("bounded compaction command", prepared.encode()?.len(), room)?;
        Ok((prepared, answer))
    }

    /// Opens connections of their own on this machine's database, from any thread: a local read
    /// sees what the machine committed, which is what it applied (replication.md, "A local read on
    /// a replica").
    pub(crate) fn reader(&self) -> crate::store::Opener<Db> {
        self.db.opener()
    }

    pub(crate) fn read_db<R>(
        &mut self,
        f: impl FnOnce(&mut Db) -> anyhow::Result<R>,
    ) -> Result<R, Refusal> {
        let (prepared, answer) = self.prepare_db(f)?;
        if !prepared.pages.is_empty() {
            return Err(api::invalid(
                "quorum read",
                "the session attempted to write",
            ));
        }
        Ok(answer)
    }

    /// Only a confirmed leader may prepare. Its owner serializes proposals until this one is
    /// applied or refused. The base fence also refuses any preparation stale by application time.
    pub fn prepare<R>(
        &mut self,
        f: impl FnOnce(&mut crate::sql::Connection<'_>) -> anyhow::Result<R>,
    ) -> Result<(Prepared, R), Refusal> {
        self.prepare_db(|db| {
            let mut connection = crate::sql::Connection::new(db);
            let value = f(&mut connection)?;
            connection.finish()?;
            Ok(value)
        })
    }

    /// The owner calls this only after hyper-durable releases the matching ReadIndex context.
    pub fn read<R>(
        &mut self,
        f: impl FnOnce(&mut crate::sql::Connection<'_>) -> anyhow::Result<R>,
    ) -> Result<R, Refusal> {
        self.require_baseline()?;
        let (answer, pages) = self
            .db
            .prepare_pages(COMMAND_BYTES, |db| {
                let mut connection = crate::sql::Connection::new(db);
                let value = f(&mut connection)?;
                connection.finish()?;
                Ok(value)
            })
            .map_err(|e| crate::database::refused(&e))?;
        if !pages.is_empty() {
            return Err(api::invalid(
                "quorum read",
                "the session attempted to write",
            ));
        }
        Ok(answer)
    }

    /// The pages every apply writes (`readsets`): the replica's marker table's and the closed
    /// time's (`closed::record`).
    pub(crate) fn applied_pages(&self) -> Result<[u32; 2], Refusal> {
        let root = |name: &str| {
            self.db
                .root_page(name)
                .map_err(|e| crate::database::refused(&e))
        };
        Ok([root("__amphora_replica")?, root("__amphora_closed")?])
    }

    /// Applies `pages` at `at`, and flushes the log with them unless `flush` says the owner will.
    fn advance(
        &mut self,
        at: Point,
        pages: &[(u32, Vec<u8>)],
        configuration: &ConfState,
        flush: bool,
    ) -> Result<(), Fatal> {
        self.advance_with(at, pages, configuration, flush, |_| Ok(()))
    }

    /// Applies `pages` and the applied marker, and `also` in the same transaction.
    fn advance_with(
        &mut self,
        at: Point,
        pages: &[(u32, Vec<u8>)],
        configuration: &ConfState,
        flush: bool,
        also: impl FnOnce(&mut Db) -> anyhow::Result<()>,
    ) -> Result<(), Fatal> {
        self.db
            .apply_pages(pages, |db| {
                marker(db, at, configuration)?;
                also(db)
            })
            .map_err(failed)?;
        if flush {
            self.log_flusher()().map_err(failed)?;
            self.flushed = at;
        }
        self.point = at;
        self.configuration = configuration.clone();
        #[cfg(test)]
        self.record_digest(at.index)?;
        Ok(())
    }

    /// The last entry applied.
    pub(crate) fn applied(&self) -> Point {
        self.point
    }

    /// The group's time at the last entry applied: this replica's closed time (replication.md,
    /// "Follower reads at a closed time"), none before its group's first stamped entry.
    pub(crate) fn closed(&self) -> Option<u64> {
        self.time
    }
    /// An entry applied: its time is the machine's.
    fn timed(&mut self, next: closed::Next) {
        self.time = next.time;
        #[cfg(test)]
        if next.lifted {
            self.lifted = self.lifted.saturating_add(1);
        }
        #[cfg(not(test))]
        let _ = next.lifted;
    }
    /// The entries applied whose stamp was lifted past the time before them.
    #[cfg(test)]
    pub(crate) fn lifted(&self) -> u64 {
        self.lifted
    }

    /// The last entry applied that no finished flush covers yet.
    pub(crate) fn unflushed(&self) -> Option<Point> {
        if self.point.index > self.flushed.index {
            Some(self.point)
        } else {
            None
        }
    }

    /// Entries applied that no finished flush covers yet: never below none, as a flush never
    /// covers more than was applied.
    pub(crate) fn flush_backlog(&self) -> u64 {
        self.point.index.saturating_sub(self.flushed.index)
    }

    /// Makes the database's filesystem `filesystem` with `free` bytes free, for a database on a
    /// simulated disk, which no filesystem holds.
    #[cfg(test)]
    pub(crate) fn stand_in_filesystem(&mut self, filesystem: u64, free: u64) {
        self.filesystem = Some(filesystem);
        self.set_free_bytes(free);
    }

    /// The directory holding the database's files, whose filesystem its log grows on.
    fn directory(&self) -> std::path::PathBuf {
        match self.db.path().parent() {
            Some(directory) if !directory.as_os_str().is_empty() => directory.to_path_buf(),
            _ => std::path::PathBuf::from("."),
        }
    }

    /// The filesystem's free bytes for this process, as statvfs's `f_bavail` (Unix) or
    /// `GetDiskFreeSpaceExW` (Windows) say them (fs4): for the flushing thread, never the owner's.
    pub(crate) fn free_space(&self) -> FreeSpace {
        let directory = self.directory();
        #[cfg(test)]
        let set = std::sync::Arc::clone(&self.free_bytes);
        Box::new(move || {
            #[cfg(test)]
            if let Some(bytes) = *set.lock().unwrap() {
                return Ok(bytes);
            }
            fs4::available_space(&directory)
        })
    }

    /// The filesystem the database's log grows on, as a number the replica logs on it share: its
    /// device on Unix (`st_dev`), its volume's serial on Windows (`GetFileInformationByHandle`,
    /// through winapi-util). On any other platform every log counts as on one filesystem, an
    /// overcount of the logs that share one, so each takes less.
    pub(crate) fn filesystem(&self) -> std::io::Result<u64> {
        #[cfg(test)]
        if let Some(filesystem) = self.filesystem {
            return Ok(filesystem);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(std::fs::metadata(self.directory())?.dev())
        }
        #[cfg(windows)]
        {
            let directory = winapi_util::Handle::from_path_any(self.directory())?;
            Ok(winapi_util::file::information(&directory)?.volume_serial_number())
        }
        #[cfg(not(any(unix, windows)))]
        {
            Ok(0)
        }
    }

    /// The frames of the database's log its next restart keeps the file's room for: past them,
    /// the restart cuts the file to nothing (`Db::keep_log`, store.md decision 31).
    pub(crate) fn keep_log(&mut self, frames: u64) {
        self.db.keep_log(frames);
    }

    /// Starts the database's log again on the machine's own connection, with no reader reading
    /// (its owner holds them back): whether it did (`Db::restart_log`).
    pub(crate) fn restart_log(&mut self) -> anyhow::Result<bool> {
        self.db.restart_log()
    }

    /// A flush of the database's log, to run on the owner's flushing thread: what was committed
    /// before it began is durable once it ends (`Db::flusher`).
    pub(crate) fn log_flusher(&self) -> crate::store::Flush {
        let flush = self.db.flusher();
        #[cfg(test)]
        let flush: crate::store::Flush = {
            let gate = std::sync::Arc::clone(&self.flush_gate);
            Box::new(move || {
                gate.pass()?;
                flush()
            })
        };
        flush
    }
    #[cfg(test)]
    pub(crate) fn flush_gate(&self) -> std::sync::Arc<FlushGate> {
        std::sync::Arc::clone(&self.flush_gate)
    }
    /// The frames the database's log holds now: its writer's own count, read from memory.
    pub(crate) fn log_frames(&self) -> anyhow::Result<u64> {
        self.db.log_frames_in_use()
    }

    /// A flush begun once `at` was applied has finished: a restart opens with `at` applied.
    pub(crate) fn flushed_through(&mut self, at: Point) {
        if at.index > self.flushed.index && at.index <= self.point.index {
            self.flushed = at;
        }
    }
}

impl StateMachine for Machine {
    type Answer = Applied;

    fn apply(&mut self, entry: &EntryRef<'_>, answers: &mut Vec<Applied>) -> Result<(), Fatal> {
        if entry.index <= self.point.index {
            return Err(failed("a committed entry was applied twice"));
        }
        self.applied_bytes = self.applied_bytes.saturating_add(entry.encoded_bytes());
        let at = Point {
            index: entry.index,
            term: entry.term,
        };
        // The entry inside its stamp, and the group's time once it is applied, written in the
        // entry's own transaction (replication.md, "Follower reads at a closed time").
        let (stamp, data) = closed::open(entry.data).map_err(failed)?;
        let before = self.time;
        let next = closed::next(before, stamp, !data.is_empty()).map_err(failed)?;
        if !data.is_empty()
            && self
                .baseline
                .apply_seed(&mut self.db, data, |db| {
                    crate::database::bootstrap_db(db)?;
                    db.exec(META)?;
                    marker(db, at, &self.configuration)?;
                    // The seed's last chunk installs an image from before any time: its table is
                    // made again whatever the machine held.
                    closed::record(db, None, next.time)
                })
                .map_err(failed)?
        {
            self.point = at;
            self.flushed = at;
            self.timed(next);
            answers.push(Applied {
                index: entry.index,
                context: entry.context.to_vec(),
                result: Ok(()),
            });
            return Ok(());
        }
        if class::is_class(data) {
            // The last compatible entry is the record; one that isn't changes nothing, on every
            // member alike, as it depends on the log alone.
            let stated = class::decode(data).map_err(failed)?;
            let supersedes = self
                .class
                .as_ref()
                .is_none_or(|record| class::compatible(record, &stated));
            let configuration = self.configuration.clone();
            if supersedes {
                self.advance_with(at, &[], &configuration, !self.replicated, |db| {
                    closed::record(db, before, next.time)?;
                    class::record(db, &stated)
                })?;
                #[cfg(test)]
                if self.class.as_ref() != Some(&stated) {
                    self.class_changes = self.class_changes.saturating_add(1);
                }
                self.class = Some(stated);
            } else {
                self.advance_with(at, &[], &configuration, !self.replicated, |db| {
                    closed::record(db, before, next.time)
                })?;
            }
            self.timed(next);
            return self.hold_to_class();
        }
        let mut result = Ok(());
        let mut pages = Vec::new();
        if !data.is_empty() {
            self.require_baseline().map_err(failed)?;
            let prepared = Prepared::decode(data).map_err(failed)?;
            if prepared.baseline.as_ref() != self.baseline.declaration() {
                return Err(failed("resolved delta baseline identity differs"));
            }
            result = on_base(prepared.base_index, prepared.base_term, self.point);
            if result.is_ok() {
                pages = prepared.pages;
            }
        }
        // Its flush is the owner's flushing thread's, in every class (replication.md, "A
        // replica's database log"): durable() stays at what a finished flush covers. A refused
        // transaction changes nothing but still takes its time: the time counts entries, and every
        // member refuses it alike.
        self.advance_with(at, &pages, &self.configuration.clone(), false, |db| {
            closed::record(db, before, next.time)
        })?;
        self.timed(next);
        if !data.is_empty() {
            answers.push(Applied {
                index: entry.index,
                context: entry.context.to_vec(),
                result,
            });
        }
        Ok(())
    }

    fn apply_change(
        &mut self,
        at: Point,
        _: &ConfChangeV2,
        configuration: &ConfState,
    ) -> Result<(), Fatal> {
        if self.writes_joins() {
            // The join rows go with the change in its own transaction (replication.md, "Joins"):
            // a change another lead replaces is never applied, so it writes nothing.
            self.advance_with(at, &[], configuration, true, |db| {
                floor::apply_joins(db, at.index, configuration)
            })?;
        } else {
            self.advance(at, &[], configuration, true)?;
        }
        self.configuration_at = at.index;
        Ok(())
    }
    fn durable(&self) -> Point {
        self.flushed
    }
    fn configuration(&self) -> &ConfState {
        &self.configuration
    }
    /// Every entry outside the replicated class, which the shell applies only on the member's durable
    /// commit (durable.md §4.1). In the class only the seed's: an ordinary entry is applied on the
    /// commit, held or classic, and a restart that finds it past the member's log opens the log at
    /// the machine's point (durable.md §4.3).
    fn acts_at_start(&self, entry: &EntryRef<'_>) -> bool {
        !self.replicated || closed::open(entry.data).is_ok_and(|(_, data)| bootstrap::is_seed(data))
    }

    fn image(&mut self, into: &mut Vec<u8>) -> Result<(Point, ConfState), Fatal> {
        self.require_baseline().map_err(failed)?;
        let room = self.snapshot_bytes().map_err(failed)?;
        let pages = self.db.image_pages(room).map_err(failed)?;
        let bytes = if let Some(declaration) = self.baseline.declaration() {
            let mut bytes = BASELINE_SNAPSHOT.to_vec();
            bytes.extend(
                postcard::to_allocvec(&BaselineSnapshot {
                    declaration: declaration.clone(),
                    pages,
                })
                .map_err(failed)?,
            );
            bytes
        } else {
            postcard::to_allocvec(&pages).map_err(failed)?
        };
        api::bound(
            "snapshot",
            bytes.len(),
            room.saturating_add(image_envelope(room)),
        )
        .map_err(failed)?;
        into.extend_from_slice(&bytes);
        self.imaged_bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        Ok((self.point, self.configuration.clone()))
    }
    /// The image this machine opened with, its pages' bytes (the postcard framing a page's id and
    /// length add is not counted): asked when a member opens on a log never compacted, so the log
    /// is weighed against the state it opened with (CT.1).
    fn image_bytes(&self) -> Option<u64> {
        Some(self.opened_bytes)
    }
    fn install(&mut self, image: &[u8], at: Point, configuration: &ConfState) -> Result<(), Fatal> {
        let room = self.snapshot_bytes().map_err(failed)?;
        api::bound(
            "snapshot",
            image.len(),
            room.saturating_add(image_envelope(room)),
        )
        .map_err(failed)?;
        let pages: Vec<(u32, Vec<u8>)> =
            if let Some(payload) = image.strip_prefix(BASELINE_SNAPSHOT) {
                let snapshot: BaselineSnapshot = postcard::from_bytes(payload).map_err(failed)?;
                if self.baseline.declaration() != Some(&snapshot.declaration) {
                    return Err(failed("snapshot baseline declaration differs"));
                }
                snapshot.pages
            } else {
                if self.baseline.declaration().is_some() {
                    return Err(failed("snapshot lacks declared baseline identity"));
                }
                postcard::from_bytes(image).map_err(failed)?
            };
        validate_pages(&pages, room).map_err(failed)?;
        if pages.first().map(|p| p.0) != Some(1) {
            return Err(failed("snapshot lacks its database header"));
        }
        self.db
            .apply_pages(&pages, |db| {
                // Inside the image's transaction: a refusal rolls it back before its marker.
                unproven_floor(db)?;
                db.exec(META)?;
                marker(db, at, configuration)?;
                self.baseline.completed_marker(db)
            })
            .map_err(failed)?;
        self.db.flusher()().map_err(failed)?;
        self.point = at;
        self.flushed = at;
        self.configuration = configuration.clone();
        self.configuration_at = at.index;
        self.baseline.completed();
        self.class = class::recorded(&self.db).map_err(failed)?;
        // The image's time is the group's at its point: an image never goes back in time, as
        // it is taken past what this member applied.
        self.time = closed::recorded(&self.db).map_err(failed)?;
        self.hold_to_class()?;
        #[cfg(test)]
        self.record_digest(at.index)?;
        Ok(())
    }
    /// The raft log is the write-ahead log (CP-T1.8, G12 step 2; replication.md, "The raft log
    /// as the WAL"): compaction asks this before it moves the raft log's start, and moves it no
    /// further than [`StateMachine::durable`], what the owner's flushing thread last made durable
    /// (I8). So nothing is flushed here, on the owner's thread: compaction waits for the engine's
    /// flush, and the raft log keeps every entry the database's log may still lose.
    fn persist(&mut self) -> Result<(), Fatal> {
        Ok(())
    }
}

#[cfg(test)]
mod envelope_tests {
    use super::*;

    fn widest_declaration() -> api::replication::Bootstrap {
        api::replication::Bootstrap {
            source_node: u64::MAX,
            source_id: [u8::MAX; 32],
        }
    }
    /// Full pages whose ids each take postcard's widest `u32`.
    fn widest_pages(budget: usize) -> Vec<(u32, Vec<u8>)> {
        let count = budget / PAGE_BYTES;
        (0..count)
            .map(|at| {
                (
                    u32::MAX - u32::try_from(count - at).unwrap() + 1,
                    vec![0; PAGE_BYTES],
                )
            })
            .collect()
    }

    /// T-EN.1.1: the widest prepared transaction a command admits: a declared baseline, base index
    /// and term at `u64::MAX`, its whole page budget in pages with the widest ids — passes when it
    /// encodes within its bound, short of it only by the list length's unused varint bytes.
    #[test]
    fn envelope_a_command_at_its_widest_fits_its_bound_exactly() {
        let pages = widest_pages(COMMAND_BYTES);
        let count = pages.len();
        let prepared = Prepared {
            base_index: u64::MAX,
            base_term: u64::MAX,
            pages,
            baseline: Some(widest_declaration()),
            reads: None,
        };
        let bytes = prepared.encode().unwrap();
        let bound = COMMAND_BYTES + COMMAND_ENVELOPE;
        assert_eq!(bound - bytes.len(), U64_MOST - varint_bytes(count));
        assert!(Prepared::decode(&bytes).is_ok());
    }

    /// T-EN.1.2: the widest image of a lowered budget, and of the full one's arithmetic — passes when
    /// it encodes within its bound, short only by the list length's unused varint bytes.
    #[test]
    fn envelope_an_image_at_its_widest_fits_its_bound_exactly() {
        let budget = 64 * PAGE_BYTES;
        let pages = widest_pages(budget);
        let count = pages.len();
        let mut bytes = BASELINE_SNAPSHOT.to_vec();
        bytes.extend(
            postcard::to_allocvec(&BaselineSnapshot {
                declaration: widest_declaration(),
                pages,
            })
            .unwrap(),
        );
        let bound = budget + image_envelope(budget);
        assert_eq!(bound - bytes.len(), U64_MOST - varint_bytes(count));
        // The full budget's envelope: its pages' ids and lengths, a few bytes a page, not a
        // mebibyte.
        let full = 256 << 20;
        let pages = full / PAGE_BYTES;
        assert_eq!(
            image_envelope(full),
            BASELINE_SNAPSHOT.len() + BOOTSTRAP_MOST + U64_MOST + pages * (U32_MOST + 2)
        );
    }

    /// T-EN.1.3: postcard's varint widths.
    #[test]
    fn envelope_varints_take_seven_bits_a_byte() {
        for (n, bytes) in [
            (0, 1),
            (127, 1),
            (128, 2),
            (4096, 2),
            (16_383, 2),
            (16_384, 3),
        ] {
            assert_eq!(varint_bytes(n), bytes, "{n}");
            assert_eq!(postcard::to_allocvec(&n).unwrap().len(), bytes, "{n}");
        }
        assert_eq!(
            postcard::to_allocvec(&widest_declaration()).unwrap().len(),
            BOOTSTRAP_MOST
        );
        assert_eq!(postcard::to_allocvec(&u32::MAX).unwrap().len(), U32_MOST);
        assert_eq!(postcard::to_allocvec(&u64::MAX).unwrap().len(), U64_MOST);
    }
}

#[cfg(test)]
mod image_room_tests {
    use super::*;
    use crate::memory::tests::{Fed, budget};

    /// SB.1: do open a machine over a database of a 128 KiB row, its process budget fed: room to
    /// spare, then 64 KiB of room, then a limit that can't be read, then room to spare under a
    /// carrier that holds 32 KiB. Expect: the image fits and a joiner is admitted while there is
    /// room; with 64 KiB, it doesn't fit and a joiner is refused naming that room; unread, the
    /// refusal is typed (no fallback); under the carrier, its bound (AC-SB.1.1).
    #[test]
    fn sb1_an_image_is_bounded_by_the_memory_budgets_room_for_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut machine = Machine::open(&dir.path().join("m.db"), ConfState::default()).unwrap();
        machine.db.exec("CREATE TABLE t(b)").unwrap();
        machine
            .db
            .exec("INSERT INTO t VALUES(randomblob(131072))")
            .unwrap();
        let fed = Fed::new(Some(1 << 30), Some(1 << 20));
        let (_budget, _unset) = budget(&fed);
        assert!(machine.image_fits().unwrap());
        machine.admits_joiner().unwrap();

        fed.set(Some((1 << 20) + (64 << 10)), Some(1 << 20));
        assert!(!machine.image_fits().unwrap());
        let refused = machine.admits_joiner();
        assert!(
            matches!(&refused, Err(Refusal::TooLarge { what, size, limit })
                if what.contains("joiner") && *size > 128 << 10 && *limit == 64 << 10),
            "{refused:?}"
        );

        fed.set(None, Some(1 << 20));
        assert!(matches!(machine.image_fits(), Err(Refusal::Unavailable(_))));
        assert!(matches!(
            machine.admits_joiner(),
            Err(Refusal::Unavailable(_))
        ));

        fed.set(Some(1 << 30), Some(1 << 20));
        machine.set_snapshot_bytes(32 << 10);
        let refused = machine.admits_joiner();
        assert!(
            matches!(&refused, Err(Refusal::TooLarge { limit, .. }) if *limit == 32 << 10),
            "{refused:?}"
        );
    }
}
